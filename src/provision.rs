//! Profile provisioning — make a `CLAUDE_CONFIG_DIR` satisfy the invariants csm
//! depends on, idempotently.
//!
//! csm switches profiles by pointing `CLAUDE_CONFIG_DIR` at a per-profile dir
//! (`~/.claude.<name>`). Claude Code stores plugins/marketplaces, session
//! transcripts AND its peer registry UNDER that dir, so a naked env swap gives
//! each profile its own plugin store, its own transcript history and its own
//! view of which sessions exist — switching profiles then breaks the
//! marketplace cache (`cache-miss`, "Run /reload-plugins"), splits session
//! history, and partitions cross-session messaging. We share all three subdirs
//! the same way: each profile's `plugins`, `projects` and `sessions` are
//! symlinked to one SSOT under `~/.claude.shared/`.
//!
//! [`ensure_profile_provisioned`] is the single definition of "a provisioned
//! profile". Every entry point that activates / launches / registers a profile
//! calls it, so the invariant is maintained without the user running anything.
//! The explicit `csm profiles bootstrap` / `doctor` verbs call the same code.
//!
//! ## Invariants (per profile `<name>` at dir `D`)
//! 1. `D` exists.
//! 2. `D/plugins` resolves to `~/.claude.shared/plugins` (the single SSOT), so
//!    every profile shares one marketplace cache.
//! 3. `D/projects` resolves to `~/.claude.shared/projects` (the single SSOT),
//!    so every profile's session transcripts are visible regardless of which
//!    profile is active (`csm`'s own session scanner and alias index depend on
//!    this — see `session::scan`/`session::alias`).
//! 4. `D/sessions` resolves to `~/.claude.shared/sessions` (the single SSOT),
//!    so Claude Code's peer registry spans profiles and cross-session
//!    messaging still finds every live session after a switch.
//!
//! ## Why `sessions` is swapped and drained instead of backed up
//!
//! The first three invariants are satisfied by [`link_dir_to_shared`], which
//! backs a diverged real directory up to `*.bak` and links to the existing
//! SSOT. That is wrong for `sessions`: its entries describe processes that are
//! running right now, and a `.bak` would take a live session's `<pid>.json` and
//! its `<pid>.<hash>.key` out of the registry mid-flight, so peers stop seeing
//! it and its messages stop authenticating.
//!
//! Moving the entries one by one and linking afterwards is no better. The
//! migration runs on the first launch after an upgrade, with that profile's
//! sessions still live, and any of them can write a new name into the half-empty
//! directory (a registration, a key publish, the fleet view's heartbeat) before
//! the directory can be removed. [`link_sessions_to_shared`] therefore swaps
//! first and moves afterwards:
//!
//! 1. Rename the real directory aside to a staging dir next to the SSOT
//!    (`.sessions-staging.<profile>.<pid>.<n>`), then create the symlink at
//!    once. From then on every lookup through the profile lands in the SSOT;
//!    only the few microseconds between the two calls are exposed, and a
//!    session's `mkdir -p` that recreates the directory in that gap is renamed
//!    aside too and the symlink retried.
//! 2. Drain the staging dir into the SSOT with `link(2)` then `unlink(2)`.
//!    `link` never replaces an existing name, so a copy a live session has just
//!    rewritten in the SSOT is never overwritten by the older staged one.
//! 3. When a name exists on both sides: identical bytes drop the staged copy;
//!    `<pid>.json`, `<pid>.<hash>.key` and `.fleetview-heartbeat` keep the newer
//!    mtime (a tie keeps the SSOT's); a `<pid>.<hash>.key.tmp.*` leftover is
//!    dropped. Any other colliding name, and anything that is not a regular
//!    file, stays in the staging dir, which is kept and reported rather than
//!    deleted. A regular file with no collision moves whatever its name, so one
//!    a newer Claude Code starts writing there is carried across rather than
//!    stranded; a directory, symlink or socket stays in staging either way.
//!
//! Every run, even one that finds the link already correct, first drains any
//! staging dir a crashed or concurrent run left behind, and a run that fails
//! after the rename-aside drains what it staged before returning the error.
//! `csm profiles doctor` lists the staging dirs that could not be emptied, and
//! `doctor --fix` drains them again.
//!
//! ## Home-floor gate
//!
//! `dir` (the profile's `CLAUDE_CONFIG_DIR`) and the SSOTs in `roots` are
//! resolved independently — `dir` from the registry / `$CLAUDE_CONFIG_DIR`,
//! `roots` from [`paths::home_dir`] — and nothing used to check the two
//! agreed. A shell with `$HOME` pointed at a sandbox but `$CLAUDE_CONFIG_DIR`
//! still naming a profile dir under the REAL home let `csm profiles
//! bootstrap` repoint that real profile's `plugins`/`projects`/`sessions` at
//! the sandbox's SSOT and move its live session registry there (2026-09-20
//! incident; manually reverted, nothing broken now). [`ensure_profile_provisioned`]
//! now refuses to provision a `dir` that is neither registered in
//! [`crate::account::ProfileMap`] nor shaped like `<home>/.claude` /
//! `<home>/.claude.<name>` under the resolved home — the exact question
//! [`crate::cas::platform::dir_is_broadcastable`] already answers for the
//! `cas -g` machine-wide floor, reused here rather than reimplemented (see
//! [`provisioning_allowed`]).
//!
//! The gate lives in [`ensure_profile_provisioned`] rather than in
//! `cmd::support::current_profile_dir` — the one call site the incident went
//! through — because `current_profile_dir` is not the only path a dir takes
//! to this function: `resolve_profile_dir`'s registry hit, the account
//! picker's winner, `cas add`/`cas set`'s explicit `<dir>` argument, and
//! `main`'s `--profile` pin all reach [`ensure_profile_provisioned`] without
//! ever calling `current_profile_dir`. Gating the one function every
//! provisioning path funnels through is the layer nothing can route around.
//!
//! Refusal is an `Err`. [`ensure_provisioned_soft`] — every implicit
//! switch/launch/register caller — already treats any `Err` as soft (warn to
//! stderr, keep going unprovisioned), so a launch never hard-fails over this;
//! the explicit `csm profiles bootstrap` / `doctor --fix` verbs surface it as
//! a loud per-profile failure instead, which is the right response to a
//! deliberately invoked provisioning command hitting a misaimed dir.
//!
//! ## Platform
//! POSIX uses `std::os::unix::fs::symlink`. On Windows, directory symlinks are
//! privilege-gated and the relaunch loop is currently disabled there, so we make
//! provisioning a no-op rather than fail — the Windows junction is delegated to
//! OS-native tooling provisioned outside this crate (the operator's private
//! deployment repo); csm treats it as a no-op. The dir-sharing logic (all three
//! subdirs alike) is unix-only for now.

use std::io;
use std::path::{Path, PathBuf};

use crate::account;
use crate::paths;

/// The shared SSOT roots every profile links into, injected so tests can point
/// them at a tempdir instead of `~/.claude.shared/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoots {
    /// `~/.claude.shared/plugins` — one marketplace cache.
    pub plugins: PathBuf,
    /// `~/.claude.shared/projects` — one transcript history.
    pub projects: PathBuf,
    /// `~/.claude.shared/sessions` — one peer registry.
    pub sessions: PathBuf,
}

impl SharedRoots {
    /// The production roots under `~/.claude.shared/`.
    pub fn production() -> Self {
        SharedRoots {
            plugins: paths::shared_plugins_dir(),
            projects: paths::session_base_dir(),
            sessions: paths::shared_sessions_dir(),
        }
    }
}

/// Outcome of provisioning one profile — reported by `doctor`, ignored by the
/// implicit callers (they only care that it didn't error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionReport {
    /// What the `plugins` link step did.
    pub plugins: LinkOutcome,
    /// What the `projects` link step did.
    pub projects: LinkOutcome,
    /// What the `sessions` link step did.
    pub sessions: LinkOutcome,
}

/// What happened to a single symlink-to-shared step.
///
/// The link-mutating variants are only constructed on unix (where
/// `link_dir_to_shared` actually manages the symlink); on non-unix the only
/// outcome is [`Skipped`](LinkOutcome::Skipped). Hence the per-variant
/// `cfg_attr` dead-code allows so the enum stays warning-clean on every target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// Already a correct symlink to the SSOT — no change.
    #[cfg_attr(not(unix), allow(dead_code))]
    AlreadyLinked,
    /// No prior entry existed; created the symlink (shared SSOT may have been
    /// created empty too).
    #[cfg_attr(not(unix), allow(dead_code))]
    Created,
    /// A real directory existed and seeded an empty SSOT, then was replaced by a
    /// symlink. Carries the seeded SSOT path.
    #[cfg_attr(not(unix), allow(dead_code))]
    SeededShared,
    /// A real directory existed but the SSOT already had content; the profile's
    /// copy was backed up (path returned) and replaced by a symlink.
    #[cfg_attr(not(unix), allow(dead_code))]
    BackedUp(PathBuf),
    /// The `sessions` axis only: a real directory was renamed aside and replaced
    /// by the symlink, and `moved` entries were drained from staging into the
    /// SSOT (a backup would strand a live session's registry entry). Also
    /// reported whenever a run drained a staging dir an earlier run left
    /// behind, whatever the link itself needed doing. `leftover` names the
    /// staging dirs that still hold entries the collision policy would not
    /// move; the link is made either way.
    #[cfg_attr(not(unix), allow(dead_code))]
    Merged {
        moved: usize,
        leftover: Vec<PathBuf>,
    },
    /// Skipped (non-unix platform — handled by Ansible/junctions instead).
    /// Only constructed on non-unix builds.
    #[cfg_attr(unix, allow(dead_code))]
    Skipped,
}

// ─── diagnosis (read-only; the `doctor` core) ──────────────────────────────────

/// The state of a profile's `plugins`, `projects` or `sessions` entry relative
/// to its shared SSOT. Read-only classification — `doctor` reports it; `--fix` calls
/// [`ensure_profile_provisioned`] to repair anything not [`Ok`](LinkState::Ok).
///
/// The non-`Ok` variants are only constructed by the unix `diagnose_profile_with`;
/// the non-unix `diagnose_profile` always reports `Ok` (linking is OS-side), so
/// they carry per-variant dead-code allows to stay warning-clean off unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// The entry is a symlink to the shared SSOT — healthy.
    Ok,
    /// No entry exists yet (will be created on provision).
    #[cfg_attr(not(unix), allow(dead_code))]
    Missing,
    /// The entry is a real directory (per-profile store, diverged from the SSOT).
    #[cfg_attr(not(unix), allow(dead_code))]
    RealDir,
    /// The entry is a symlink, but to the wrong target.
    #[cfg_attr(not(unix), allow(dead_code))]
    WrongLink(PathBuf),
    /// The entry is a symlink to the right SSOT path, but the SSOT directory
    /// itself is gone. Reading through the link fails, and Bun's `mkdir -p`
    /// fails with `EEXIST` on it, so Claude Code can neither register a session
    /// nor write a transcript there. Provisioning recreates the target.
    #[cfg_attr(not(unix), allow(dead_code))]
    Dangling,
    /// The entry is a regular file (or other non-dir) — unexpected.
    #[cfg_attr(not(unix), allow(dead_code))]
    NotADir,
}

impl LinkState {
    /// Is this entry's link already healthy (no action needed)?
    pub fn is_ok(&self) -> bool {
        matches!(self, LinkState::Ok)
    }
}

/// Read-only diagnosis of one profile — the pure core of `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDiagnosis {
    /// The profile dir exists on disk.
    pub dir_exists: bool,
    /// State of the `plugins` → shared SSOT link.
    pub plugins: LinkState,
    /// State of the `projects` → shared SSOT link.
    pub projects: LinkState,
    /// State of the `sessions` → shared SSOT link.
    pub sessions: LinkState,
}

impl ProfileDiagnosis {
    /// Is the profile fully provisioned (nothing for `--fix` to do)?
    pub fn is_healthy(&self) -> bool {
        self.dir_exists && self.plugins.is_ok() && self.projects.is_ok() && self.sessions.is_ok()
    }
}

/// Diagnose profile `dir` against the production shared SSOTs.
#[cfg(unix)]
pub fn diagnose_profile(dir: &Path) -> ProfileDiagnosis {
    diagnose_profile_with(dir, &SharedRoots::production())
}

/// Non-unix: dir linking is delegated to OS-native tooling, so the diagnosis
/// reports only whether the profile dir exists and treats every link as `Ok`.
#[cfg(not(unix))]
pub fn diagnose_profile(dir: &Path) -> ProfileDiagnosis {
    ProfileDiagnosis {
        dir_exists: dir.is_dir(),
        plugins: LinkState::Ok,
        projects: LinkState::Ok,
        sessions: LinkState::Ok,
    }
}

/// Classify a single `link` against its expected `shared` SSOT target. Shared
/// with [`crate::homeguard`], which classifies `~/.claude/projects` by exactly
/// the same rules.
#[cfg(unix)]
pub(crate) fn classify_link(link: &Path, shared: &Path) -> LinkState {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(link) {
            Ok(target) if links_match(&target, link, shared) => LinkState::Ok,
            Ok(target) => LinkState::WrongLink(target),
            Err(_) => LinkState::WrongLink(PathBuf::new()),
        },
        Ok(meta) if meta.is_dir() => LinkState::RealDir,
        Ok(_) => LinkState::NotADir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => LinkState::Missing,
        Err(_) => LinkState::NotADir,
    }
}

/// [`classify_link`], plus the one check it leaves out: a link to the right
/// path whose target directory is gone is [`LinkState::Dangling`], not `Ok`.
/// `classify_link` stays lexical because [`crate::homeguard`] makes that
/// judgement itself from a separate probe of the shared dir.
#[cfg(unix)]
fn classify_profile_link(link: &Path, shared: &Path) -> LinkState {
    match classify_link(link, shared) {
        LinkState::Ok if !shared.is_dir() => LinkState::Dangling,
        state => state,
    }
}

/// [`diagnose_profile`] with the SSOTs injected (testable seam). Pure: only
/// reads the filesystem, never mutates.
#[cfg(unix)]
pub fn diagnose_profile_with(dir: &Path, roots: &SharedRoots) -> ProfileDiagnosis {
    ProfileDiagnosis {
        dir_exists: dir.is_dir(),
        plugins: classify_profile_link(&dir.join("plugins"), &roots.plugins),
        projects: classify_profile_link(&dir.join("projects"), &roots.projects),
        sessions: classify_profile_link(&dir.join("sessions"), &roots.sessions),
    }
}

/// Is `dir` safe to provision — does it belong to the same `$HOME` (or
/// registry) that the shared SSOTs are derived from?
///
/// This is the exact question [`crate::cas::platform::dir_is_broadcastable`]
/// answers for the `cas -g` machine-wide floor — is `dir` a legitimate
/// profile location, one some profile is registered at, or one shaped like
/// `<home>/.claude` / `<home>/.claude.<name>`? Reused rather than
/// re-implemented; see that function's doc and tests for the shape rules.
///
/// `registry` is `None` when [`account::ProfileMap::load`] itself failed
/// (corrupt/unreadable `profiles.json`), and that is a refusal too, never a
/// pass: an unreadable registry can neither confirm nor deny that `dir` is
/// registered, so it answers "I don't know", not "go ahead". See the
/// module-level *Home-floor gate* section for why this check exists.
fn provisioning_allowed(
    registry: Option<&account::ProfileMap>,
    home: Option<&Path>,
    dir: &Path,
) -> bool {
    let Some(registry) = registry else {
        return false;
    };
    crate::cas::platform::dir_is_broadcastable(registry, home, &dir.to_string_lossy())
}

/// Ensure profile `name` at `dir` satisfies the provisioning invariants.
///
/// Idempotent: safe to call on every switch/launch/register. Returns a report
/// of what each step did (for `doctor`); implicit callers discard it.
///
/// `dir` is the profile's `CLAUDE_CONFIG_DIR` (from the registry, never a
/// literal). `name` is informational (kept for future per-name steps and for
/// error context).
///
/// Gated by [`provisioning_allowed`] first — see the module-level *Home-floor
/// gate* section. A `dir` that fails the gate is refused with an `Err`
/// instead of being linked into SSOTs that may belong to a different home.
pub fn ensure_profile_provisioned(name: &str, dir: &Path) -> io::Result<ProvisionReport> {
    let registry = account::ProfileMap::load();
    let home = paths::home_dir();
    if !provisioning_allowed(registry.as_ref().ok(), home.as_deref(), dir) {
        let home_desc = match &home {
            Some(h) => h.display().to_string(),
            None => "<no $HOME>".to_owned(),
        };
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "provision[{name}]: refusing to provision {} — it is not a registered \
                 profile dir and not shaped like {home_desc}/.claude* under the resolved \
                 $HOME; this profile dir may belong to a different $HOME than the shared \
                 SSOT (see the Home-floor gate note in provision.rs)",
                dir.display()
            ),
        ));
    }
    ensure_profile_provisioned_with(name, dir, &SharedRoots::production())
}

/// One `<profile>/<sub>` → SSOT link step, with the profile-scoped error
/// context every provisioning failure carries.
fn link_step(name: &str, sub: &str, result: io::Result<LinkOutcome>) -> io::Result<LinkOutcome> {
    result.map_err(|e| io::Error::new(e.kind(), format!("provision[{name}]: {sub}: {e}")))
}

/// [`ensure_profile_provisioned`] with the shared SSOTs injected — the testable
/// seam (mirrors `ProfileMap::default_name_with`). Production passes
/// [`SharedRoots::production`]; tests pass tempdir paths.
pub fn ensure_profile_provisioned_with(
    name: &str,
    dir: &Path,
    roots: &SharedRoots,
) -> io::Result<ProvisionReport> {
    // 1. The profile dir itself.
    std::fs::create_dir_all(dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("provision[{name}]: create {} failed: {e}", dir.display()),
        )
    })?;

    // 2. Each subdir → its shared SSOT. `sessions` is swapped and drained
    //    rather than backed up (see the module doc).
    let plugins = link_step(
        name,
        "plugins",
        link_dir_to_shared(&dir.join("plugins"), &roots.plugins),
    )?;
    let projects = link_step(
        name,
        "projects",
        link_dir_to_shared(&dir.join("projects"), &roots.projects),
    )?;
    let sessions = link_step(
        name,
        "sessions",
        link_sessions_to_shared(&dir.join("sessions"), &roots.sessions),
    )?;

    Ok(ProvisionReport {
        plugins,
        projects,
        sessions,
    })
}

/// Best-effort provisioning that never propagates an error — for the hot
/// switch/launch path, where a provisioning hiccup must not block the launch.
/// Logs to stderr on failure and continues. The display name is derived from
/// the dir leaf (`~/.claude.<name>` → `<name>`) for the warning message only.
pub fn ensure_provisioned_soft(dir: &Path) {
    let name = display_name_for(dir);
    if let Err(e) = ensure_profile_provisioned(&name, dir) {
        eprintln!("csm: warning: profile provisioning skipped: {e}");
    }
    crate::homeguard::ensure_home_shim_soft();
}

/// Derive a human display name from a profile dir leaf: `~/.claude.<name>` →
/// `<name>`; otherwise the leaf, or `"?"`. Used only for diagnostics — the
/// registry name is authoritative everywhere it matters.
fn display_name_for(dir: &Path) -> String {
    let leaf = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    leaf.strip_prefix(".claude.")
        .map(str::to_owned)
        .unwrap_or(leaf)
}

/// Ensure `link` is a symlink to `shared` (the SSOT), handling every prior state
/// idempotently:
///
/// - `link` is already a symlink pointing at `shared` → [`LinkOutcome::AlreadyLinked`]
///   (and `shared` is recreated if it had gone missing, so the link does not
///   dangle).
/// - `link` is a symlink to something else → repointed → [`LinkOutcome::Created`].
/// - `link` does not exist → create `shared` (empty if absent) and symlink →
///   [`LinkOutcome::Created`].
/// - `link` is a real directory and `shared` does NOT exist → move `link` to
///   become `shared` (seed), then symlink → [`LinkOutcome::SeededShared`].
/// - `link` is a real directory and `shared` exists → back up `link` to a
///   sibling `*.bak.<n>` and symlink → [`LinkOutcome::BackedUp`].
///
/// On non-unix, returns [`LinkOutcome::Skipped`] without touching the fs.
#[cfg(unix)]
pub fn link_dir_to_shared(link: &Path, shared: &Path) -> io::Result<LinkOutcome> {
    use std::os::unix::fs::symlink;

    // Already a symlink? Compare its target against `shared` (canonicalized so a
    // relative/equivalent target still counts as correct).
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let cur = std::fs::read_link(link)?;
            if links_match(&cur, link, shared) {
                // A dangling link reads as a match too (lexical fallback), and
                // Claude Code's `mkdir -p` fails through one; put the target back.
                std::fs::create_dir_all(shared)?;
                return Ok(LinkOutcome::AlreadyLinked);
            }
            // Wrong target — repoint. Ensure the SSOT exists first.
            std::fs::create_dir_all(shared)?;
            std::fs::remove_file(link)?;
            symlink(shared, link)?;
            return Ok(LinkOutcome::Created);
        }
        Ok(meta) if meta.is_dir() => {
            // Real directory. Seed the SSOT from it if the SSOT is absent;
            // otherwise back the profile copy up and link to the existing SSOT.
            if !shared.exists() {
                if let Some(parent) = shared.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(link, shared)?;
                symlink(shared, link)?;
                return Ok(LinkOutcome::SeededShared);
            }
            let backup = backup_path(link)?;
            std::fs::rename(link, &backup)?;
            symlink(shared, link)?;
            return Ok(LinkOutcome::BackedUp(backup));
        }
        Ok(_) => {
            // A regular file (or other) sits where the dir should be — back it up.
            std::fs::create_dir_all(shared)?;
            let backup = backup_path(link)?;
            std::fs::rename(link, &backup)?;
            symlink(shared, link)?;
            return Ok(LinkOutcome::BackedUp(backup));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => { /* fall through to create */ }
        Err(e) => return Err(e),
    }

    // Nothing at `link`. Create the SSOT (empty if needed) and symlink.
    std::fs::create_dir_all(shared)?;
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    symlink(shared, link)?;
    Ok(LinkOutcome::Created)
}

/// Non-unix: provisioning the symlink is delegated to the OS-native tooling
/// (Ansible junctions); the binary does not attempt it.
#[cfg(not(unix))]
pub fn link_dir_to_shared(_link: &Path, _shared: &Path) -> io::Result<LinkOutcome> {
    Ok(LinkOutcome::Skipped)
}

/// Ensure `link` (a profile's `sessions`) is a symlink to `shared`, the peer
/// registry every profile shares, without stranding a live session's entry.
/// See the module doc for the swap-then-drain order and the collision policy.
///
/// - `link` already correct → [`LinkOutcome::AlreadyLinked`].
/// - `link` missing, or a symlink to something else → linked →
///   [`LinkOutcome::Created`].
/// - `link` a regular file → backed up as [`link_dir_to_shared`] does →
///   [`LinkOutcome::BackedUp`].
/// - `link` a real directory → renamed aside, linked, drained →
///   [`LinkOutcome::Merged`].
///
/// The first three become [`LinkOutcome::Merged`] too whenever the run drained
/// a staging dir an interrupted or concurrent run left behind, so a stranded
/// entry is reported by whichever run recovers it rather than only by `doctor`.
///
/// `shared` is created `0700` (as Claude Code creates it) when absent; an
/// existing one is never chmodded. The swap needs the profile dir and `shared`
/// on one filesystem: across volumes the rename-aside fails with `EXDEV`,
/// nothing has changed, and the error is returned. A failure after the
/// rename-aside puts the directory back and, failing that, still drains it into
/// the SSOT — an error never leaves a live entry parked in staging.
#[cfg(unix)]
pub fn link_sessions_to_shared(link: &Path, shared: &Path) -> io::Result<LinkOutcome> {
    let staging_parent = shared.parent().unwrap_or(Path::new("."));
    link_sessions_with(link, shared, staging_parent, &|| {})
}

/// Non-unix: as with [`link_dir_to_shared`], linking is delegated OS-side.
#[cfg(not(unix))]
pub fn link_sessions_to_shared(_link: &Path, _shared: &Path) -> io::Result<LinkOutcome> {
    Ok(LinkOutcome::Skipped)
}

/// Prefix of a staging dir under the SSOT's parent. It sits outside every
/// `sessions` dir, so Claude Code's own registry scans never see it.
const STAGING_PREFIX: &str = ".sessions-staging.";

/// How many times a real `sessions` dir is renamed aside before giving up. Only
/// a session's `mkdir -p` landing in the microseconds between the rename and
/// the symlink recreates it, so a second attempt almost always wins.
#[cfg(unix)]
const SWAP_ATTEMPTS: usize = 3;

/// Re-listings of one staging dir. The second pass catches a file whose
/// `open(O_CREAT)` resolved into the old directory just before the swap; no
/// lookup lands there once the symlink exists.
#[cfg(unix)]
const DRAIN_PASSES: usize = 3;

/// [`link_sessions_to_shared`] with the staging parent injected and a `gap`
/// hook run between the rename-aside and the symlink (the testable seam;
/// production passes a no-op).
#[cfg(unix)]
fn link_sessions_with(
    link: &Path,
    shared: &Path,
    staging_parent: &Path,
    gap: &dyn Fn(),
) -> io::Result<LinkOutcome> {
    // 0. The SSOT exists (0700 on first creation, which also repairs a dangling
    //    link), and whatever an earlier run left in staging is drained first.
    if let Some(parent) = shared.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(staging_parent)?;
    ensure_private_dir(shared)?;
    let mut drained = Drain::default();
    for stage in staging_dirs_in(staging_parent) {
        drained.absorb(drain_staging_dir(&stage, shared));
    }

    // 1–3. Classify and swap, re-classifying whenever a concurrent csm or a
    //      registering session changed `link` under us. Nothing here returns
    //      early: every exit, error included, falls through to step 4, because
    //      a staged dir holds live registry entries that must not wait for the
    //      next run.
    let mut staged = Vec::new();
    let mut outcome = None;
    let mut failure = None;
    for _ in 0..SWAP_ATTEMPTS * 2 + 2 {
        match swap_once(link, shared, staging_parent, gap, &mut staged) {
            Ok(Step::Done(done)) => {
                outcome = Some(done);
                break;
            }
            Ok(Step::Retry) => continue,
            Ok(Step::Exhausted) => break,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    // 4. Drain what this run staged. Even when the link could not be made, the
    //    staged entries are moved now rather than left for the next run.
    for stage in &staged {
        drained.absorb(drain_staging_dir(stage, shared));
    }
    if let Some(e) = failure {
        return Err(e);
    }
    let Some(outcome) = outcome else {
        return Err(io::Error::other(format!(
            "{} kept changing under the swap; gave up after {SWAP_ATTEMPTS} attempts \
             (whatever was staged has been drained into {})",
            link.display(),
            shared.display()
        )));
    };
    Ok(match outcome {
        // Any run that moved something, or that left a staging dir behind,
        // reports it — whatever the link itself needed doing. Only a run that
        // touched no staging dir at all keeps the plain outcome.
        LinkOutcome::AlreadyLinked | LinkOutcome::Created | LinkOutcome::BackedUp(_)
            if staged.is_empty() && drained.is_noop() =>
        {
            outcome
        }
        _ => LinkOutcome::Merged {
            moved: drained.moved,
            leftover: drained.leftover,
        },
    })
}

/// What one pass of the classify-and-swap loop settled.
#[cfg(unix)]
enum Step {
    /// `link` is in its final state.
    Done(LinkOutcome),
    /// Something changed under us; classify again.
    Retry,
    /// A session keeps recreating the directory; stop and report.
    Exhausted,
}

/// One pass of steps 1–3: classify `link`, and swap a real directory aside into
/// `staged` before linking. Returning an error here is not a dead end — the
/// caller still drains `staged` before it propagates the error.
#[cfg(unix)]
fn swap_once(
    link: &Path,
    shared: &Path,
    staging_parent: &Path,
    gap: &dyn Fn(),
    staged: &mut Vec<PathBuf>,
) -> io::Result<Step> {
    use std::os::unix::fs::symlink;

    let meta = match std::fs::symlink_metadata(link) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let made = match meta {
        Some(meta) if meta.file_type().is_symlink() => {
            match std::fs::read_link(link) {
                Ok(cur) if links_match(&cur, link, shared) => {
                    return Ok(Step::Done(LinkOutcome::AlreadyLinked));
                }
                Ok(_) => {}
                // A concurrent csm moved the link aside between the two calls.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Step::Retry),
                Err(e) => return Err(e),
            }
            remove_if_present(link)?;
            symlink(shared, link).map(|()| LinkOutcome::Created)
        }
        Some(meta) if meta.is_dir() => {
            if staged.len() == SWAP_ATTEMPTS {
                return Ok(Step::Exhausted);
            }
            let stage = staging_path(staging_parent, link)?;
            match std::fs::rename(link, &stage) {
                Ok(()) => {}
                // A concurrent csm swapped it first; see what it left.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Step::Retry),
                Err(e) => return Err(e),
            }
            if std::fs::symlink_metadata(&stage).is_ok_and(|m| m.file_type().is_symlink()) {
                // Between the lstat and the rename a concurrent csm put its
                // symlink here, and that is what moved. Drop it and re-link.
                remove_if_present(&stage)?;
                return Ok(Step::Retry);
            }
            staged.push(stage);
            gap();
            symlink(shared, link)
                .map(|()| LinkOutcome::Merged {
                    moved: 0,
                    leftover: Vec::new(),
                })
                .inspect_err(|e| {
                    // Not the gap race but a real failure (a read-only or full
                    // filesystem): put the directory back where it was, so a
                    // run that cannot link leaves the profile as it found it.
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        restore_staged(staged, link);
                    }
                })
        }
        Some(_) => {
            let backup = backup_path(link)?;
            std::fs::rename(link, &backup)?;
            symlink(shared, link).map(|()| LinkOutcome::BackedUp(backup))
        }
        None => {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            symlink(shared, link).map(|()| LinkOutcome::Created)
        }
    };
    match made {
        Ok(done) => Ok(Step::Done(done)),
        // Something took the name between our lstat and our symlink: a
        // session's `mkdir -p`, or a concurrent csm's link. Look again.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Step::Retry),
        Err(e) => Err(e),
    }
}

/// Rename the dir this pass just staged back to `link`. Best effort: if it
/// fails the path stays in `staged` and its entries are drained into the SSOT
/// instead, which is where every lookup would have found them anyway.
#[cfg(unix)]
fn restore_staged(staged: &mut Vec<PathBuf>, link: &Path) {
    let Some(stage) = staged.pop() else { return };
    if std::fs::rename(&stage, link).is_err() {
        staged.push(stage);
    }
}

/// Create `dir` with mode `0700` if it does not exist. An existing directory
/// is left exactly as it is: its mode is the user's (or Claude Code's) choice.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// A fresh staging path for `link`:
/// `<staging_parent>/.sessions-staging.<profile-dir-leaf>.<pid>.<n>`. The pid
/// keeps concurrent csm processes apart; the counter keeps threads of one
/// process apart (tests run provisioning concurrently in-process).
#[cfg(unix)]
fn staging_path(staging_parent: &Path, link: &Path) -> io::Result<PathBuf> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    let leaf = link
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "profile".to_owned());
    let pid = std::process::id();
    for _ in 0..1000 {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = staging_parent.join(format!("{STAGING_PREFIX}{leaf}.{pid}.{n}"));
        // A recycled pid may have left this exact name behind.
        if matches!(std::fs::symlink_metadata(&candidate), Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free staging name under {}", staging_parent.display()),
    ))
}

/// The staging dirs directly under `parent`, sorted. Only real directories
/// count: a symlink named like one is never followed, so a drain can never be
/// pointed back at the SSOT itself. Read-only.
fn staging_dirs_in(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
        .map(|e| e.path())
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect();
    dirs.sort();
    dirs
}

/// Staging dirs still sitting under the production SSOT's parent. Most are
/// transient — a run that died between the swap and the drain leaves one, and
/// the next provisioning drains it — so `csm profiles doctor` reports them
/// without calling any profile unhealthy: every link is already correct.
/// Read-only.
pub fn leftover_sessions_staging() -> Vec<PathBuf> {
    let shared = paths::shared_sessions_dir();
    shared.parent().map(staging_dirs_in).unwrap_or_default()
}

/// Drain the staging dirs [`leftover_sessions_staging`] reports into the SSOT
/// and return the ones still not empty — what `csm profiles doctor --fix` runs
/// so a dir an interrupted run left behind is merged even when every profile's
/// link is already correct (in which case `--fix` provisions nothing). Touches
/// no profile's own entry.
#[cfg(unix)]
pub fn drain_leftover_sessions_staging() -> Vec<PathBuf> {
    drain_staging_under(&paths::shared_sessions_dir())
}

/// Non-unix: the sessions axis is skipped entirely, so there is nothing to drain.
#[cfg(not(unix))]
pub fn drain_leftover_sessions_staging() -> Vec<PathBuf> {
    Vec::new()
}

/// [`drain_leftover_sessions_staging`] with the SSOT injected (testable seam).
#[cfg(unix)]
fn drain_staging_under(shared: &Path) -> Vec<PathBuf> {
    let Some(parent) = shared.parent() else {
        return Vec::new();
    };
    let stages = staging_dirs_in(parent);
    if stages.is_empty() || ensure_private_dir(shared).is_err() {
        return stages;
    }
    let mut drain = Drain::default();
    for stage in stages {
        drain.absorb(drain_staging_dir(&stage, shared));
    }
    drain.leftover
}

/// What draining staging dirs achieved.
#[cfg(unix)]
#[derive(Debug, Default)]
struct Drain {
    /// Entries now in the SSOT that came from staging.
    moved: usize,
    /// Staging dirs still holding something after the drain.
    leftover: Vec<PathBuf>,
}

#[cfg(unix)]
impl Drain {
    fn absorb(&mut self, other: Drain) {
        self.moved += other.moved;
        self.leftover.extend(other.leftover);
    }

    fn is_noop(&self) -> bool {
        self.moved == 0 && self.leftover.is_empty()
    }
}

/// What happened to one staged entry.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum EntryFate {
    /// Now in the SSOT.
    Moved,
    /// The SSOT's copy won; the staged copy was deleted.
    Dropped,
    /// Left in staging (non-regular, unrecognised collision, or an I/O error).
    Kept,
    /// Already gone: a concurrent drain took it, or its session exited.
    Gone,
}

/// Move every regular file of `stage` into `shared`, then remove `stage` if it
/// is empty. Never fails: an entry that cannot be moved stays in `stage`, which
/// is then reported as leftover, and `NotFound` anywhere means another drain or
/// an exiting session got there first.
#[cfg(unix)]
fn drain_staging_dir(stage: &Path, shared: &Path) -> Drain {
    let mut drain = Drain::default();
    // A staging dir is only ever a real directory csm renamed there. Anything
    // else (a symlink, which could resolve to the SSOT itself) is not touched.
    match std::fs::symlink_metadata(stage) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            drain.leftover.push(stage.to_path_buf());
            return drain;
        }
        Err(_) => return drain,
    }
    for _ in 0..DRAIN_PASSES {
        let entries = match std::fs::read_dir(stage) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return drain,
            Err(_) => break,
        };
        let mut acted = false;
        for entry in entries.flatten() {
            let from = entry.path();
            match std::fs::symlink_metadata(&from) {
                Ok(meta) if meta.file_type().is_file() => {}
                // Directories, symlinks and sockets are never moved.
                _ => continue,
            }
            let name = entry.file_name();
            match move_staged_file(&from, &shared.join(&name), &name.to_string_lossy()) {
                EntryFate::Moved => {
                    drain.moved += 1;
                    acted = true;
                }
                EntryFate::Dropped => acted = true,
                EntryFate::Kept | EntryFate::Gone => {}
            }
        }
        if !acted {
            break;
        }
    }
    match std::fs::remove_dir(stage) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => drain.leftover.push(stage.to_path_buf()),
    }
    drain
}

/// Move one staged regular file `from` to `to` without ever replacing a newer
/// copy: `link(2)` refuses an existing name, and only then does the collision
/// policy decide.
#[cfg(unix)]
fn move_staged_file(from: &Path, to: &Path, name: &str) -> EntryFate {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            let _ = remove_if_present(from);
            EntryFate::Moved
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => EntryFate::Gone,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => resolve_collision(from, to, name),
        Err(_) => EntryFate::Kept,
    }
}

/// `name` exists both in staging (`from`) and in the SSOT (`to`). Apply the
/// collision policy from the module doc.
#[cfg(unix)]
fn resolve_collision(from: &Path, to: &Path, name: &str) -> EntryFate {
    let drop_staged = || match remove_if_present(from) {
        Ok(()) => EntryFate::Dropped,
        Err(_) => EntryFate::Kept,
    };
    let kind = RegistryName::classify(name);
    if kind == RegistryName::KeyTmp || same_bytes(from, to) {
        return drop_staged();
    }
    if kind == RegistryName::Other {
        return EntryFate::Kept;
    }
    // Newer mtime wins; a tie keeps the SSOT's copy.
    let staged = match std::fs::metadata(from).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return EntryFate::Gone,
        Err(_) => return EntryFate::Kept,
    };
    let staged_is_newer = match std::fs::metadata(to).and_then(|m| m.modified()) {
        Ok(t) => staged > t,
        // The SSOT's copy vanished between the link attempt and now (its
        // session exited). One more no-replace link settles it either way.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return match std::fs::hard_link(from, to) {
                Ok(()) => {
                    let _ = remove_if_present(from);
                    EntryFate::Moved
                }
                Err(_) => EntryFate::Kept,
            };
        }
        Err(_) => return EntryFate::Kept,
    };
    if !staged_is_newer {
        return drop_staged();
    }
    match std::fs::rename(from, to) {
        Ok(()) => EntryFate::Moved,
        Err(e) if e.kind() == io::ErrorKind::NotFound => EntryFate::Gone,
        Err(_) => EntryFate::Kept,
    }
}

/// `remove_file`, treating an already-missing file as success.
#[cfg(unix)]
fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Do `a` and `b` hold the same bytes? The same inode (a concurrent drain
/// already linked it across) always does. Any read error answers no.
#[cfg(unix)]
fn same_bytes(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
        if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
            return true;
        }
        if ma.len() != mb.len() {
            return false;
        }
    }
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The kinds of name Claude Code writes into its registry dir, as far as the
/// collision policy needs to tell them apart.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryName {
    /// `<pid>.json`: one live session's record.
    PidJson,
    /// `<pid>.<64 hex>.key`: the session's inbox key.
    Key,
    /// `<pid>.<64 hex>.key.tmp.<hex>`: a key write that never got renamed.
    KeyTmp,
    /// `.fleetview-heartbeat`: a freshness marker, rewritten every few seconds.
    Heartbeat,
    /// Anything else.
    Other,
}

#[cfg(unix)]
impl RegistryName {
    fn classify(name: &str) -> Self {
        fn digits(s: &str) -> bool {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
        }
        fn lower_hex(s: &str) -> bool {
            !s.is_empty() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        if name == ".fleetview-heartbeat" {
            return RegistryName::Heartbeat;
        }
        if name.strip_suffix(".json").is_some_and(digits) {
            return RegistryName::PidJson;
        }
        // `<pid>.<hash>.key[.tmp.<hex>]`
        let mut parts = name.splitn(4, '.');
        let (Some(pid), Some(hash), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            return RegistryName::Other;
        };
        if !digits(pid) || hash.len() != 64 || !lower_hex(hash) {
            return RegistryName::Other;
        }
        match (rest, parts.next()) {
            ("key", None) => RegistryName::Key,
            ("key", Some(tail)) if tail.strip_prefix("tmp.").is_some_and(lower_hex) => {
                RegistryName::KeyTmp
            }
            _ => RegistryName::Other,
        }
    }
}

/// Does the existing symlink target `cur` (as read from `link`) point at the
/// same location as `shared`? Resolves both to absolutes for the comparison so
/// an absolute target matches regardless of how it was written.
#[cfg(unix)]
fn links_match(cur: &Path, link: &Path, shared: &Path) -> bool {
    let resolved = if cur.is_absolute() {
        cur.to_path_buf()
    } else {
        // Relative symlink target is resolved against the link's parent dir.
        link.parent().unwrap_or(Path::new(".")).join(cur)
    };
    // Prefer canonicalization (follows the target); fall back to lexical equality
    // when either side can't be canonicalized (e.g. the SSOT not yet created).
    match (resolved.canonicalize(), shared.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => resolved == shared,
    }
}

/// Pick a non-colliding `*.bak.<n>` sibling for `path`. Tries `.bak`, then
/// `.bak.1`, `.bak.2`, … up to a bound, erroring if all are taken.
#[cfg(unix)]
fn backup_path(path: &Path) -> io::Result<PathBuf> {
    let base = path.as_os_str().to_owned();
    for n in 0..1000u32 {
        let mut candidate = base.clone();
        if n == 0 {
            candidate.push(".bak");
        } else {
            candidate.push(format!(".bak.{n}"));
        }
        let p = PathBuf::from(candidate);
        if !p.exists() {
            return Ok(p);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free backup slot for {}", path.display()),
    ))
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    /// `(link, shared)` paths under a fresh tempdir. The link lives in a
    /// `profile/` subdir, the shared SSOT in a sibling `shared/plugins`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let profile = td.path().join("profile");
        fs::create_dir_all(&profile).unwrap();
        let link = profile.join("plugins");
        let shared = td.path().join("shared").join("plugins");
        (td, link, shared)
    }

    /// The shared SSOT roots under `td`.
    fn shared_dirs(td: &tempfile::TempDir) -> SharedRoots {
        SharedRoots {
            plugins: td.path().join("shared").join("plugins"),
            projects: td.path().join("shared").join("projects"),
            sessions: td.path().join("shared").join("sessions"),
        }
    }

    fn is_symlink_to(link: &Path, shared: &Path) -> bool {
        let meta = fs::symlink_metadata(link).unwrap();
        if !meta.file_type().is_symlink() {
            return false;
        }
        fs::read_link(link).unwrap() == shared
    }

    #[test]
    fn none_creates_symlink_and_empty_ssot() {
        let (_td, link, shared) = fixture();
        let out = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(out, LinkOutcome::Created);
        assert!(is_symlink_to(&link, &shared), "link must point at SSOT");
        assert!(shared.is_dir(), "SSOT dir must exist (empty)");
    }

    #[test]
    fn correct_symlink_is_noop() {
        let (_td, link, shared) = fixture();
        fs::create_dir_all(&shared).unwrap();
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let out = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(out, LinkOutcome::AlreadyLinked);
        assert!(is_symlink_to(&link, &shared));
    }

    #[test]
    fn wrong_symlink_is_repointed() {
        let (td, link, shared) = fixture();
        let other = td.path().join("other");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let out = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(out, LinkOutcome::Created);
        assert!(is_symlink_to(&link, &shared), "must repoint at the SSOT");
    }

    #[test]
    fn real_dir_seeds_absent_ssot() {
        let (_td, link, shared) = fixture();
        // A real plugins dir with a marker file; SSOT does not exist yet.
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join("marker.json"), b"{}").unwrap();
        assert!(!shared.exists());

        let out = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(out, LinkOutcome::SeededShared);
        assert!(is_symlink_to(&link, &shared));
        // The marker moved into the SSOT (content preserved, not lost).
        assert!(
            shared.join("marker.json").exists(),
            "content must seed SSOT"
        );
        // And is visible through the link.
        assert!(link.join("marker.json").exists());
    }

    #[test]
    fn real_dir_backs_up_when_ssot_exists() {
        let (_td, link, shared) = fixture();
        // SSOT already has content (the canonical store).
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("canonical.json"), b"{}").unwrap();
        // The profile has a divergent real dir.
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join("divergent.json"), b"{}").unwrap();

        let out = link_dir_to_shared(&link, &shared).unwrap();
        match &out {
            LinkOutcome::BackedUp(backup) => {
                assert!(backup.exists(), "backup dir must exist");
                assert!(
                    backup.join("divergent.json").exists(),
                    "divergent content must be preserved in backup"
                );
            }
            other => panic!("expected BackedUp, got {other:?}"),
        }
        assert!(is_symlink_to(&link, &shared));
        // Through the link we now see the canonical SSOT content.
        assert!(link.join("canonical.json").exists());
    }

    #[test]
    fn regular_file_at_link_is_backed_up() {
        let (_td, link, shared) = fixture();
        fs::write(&link, b"not a dir").unwrap();
        let out = link_dir_to_shared(&link, &shared).unwrap();
        assert!(matches!(out, LinkOutcome::BackedUp(_)));
        assert!(is_symlink_to(&link, &shared));
    }

    #[test]
    fn idempotent_second_call_is_noop() {
        let (_td, link, shared) = fixture();
        let first = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(first, LinkOutcome::Created);
        let second = link_dir_to_shared(&link, &shared).unwrap();
        assert_eq!(second, LinkOutcome::AlreadyLinked, "second call is a no-op");
    }

    #[test]
    fn ensure_profile_creates_dir_and_links() {
        let td = tempfile::tempdir().unwrap();
        // Profile dir does NOT exist yet — provisioning must create it.
        let dir = td.path().join(".claude.example");
        let roots = shared_dirs(&td);
        assert!(!dir.exists());

        let report = ensure_profile_provisioned_with("example", &dir, &roots).unwrap();
        assert!(dir.is_dir(), "profile dir must be created");
        assert_eq!(report.plugins, LinkOutcome::Created);
        assert_eq!(report.projects, LinkOutcome::Created);
        assert_eq!(report.sessions, LinkOutcome::Created);
        assert!(is_symlink_to(&dir.join("plugins"), &roots.plugins));
        assert!(is_symlink_to(&dir.join("projects"), &roots.projects));
        assert!(is_symlink_to(&dir.join("sessions"), &roots.sessions));
        // Guard against a copy-paste bug linking two subdirs to the same SSOT.
        assert_ne!(roots.plugins, roots.projects);
        assert_ne!(roots.projects, roots.sessions);
        assert_ne!(roots.plugins, roots.sessions);
    }

    #[test]
    fn ensure_profile_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(".claude.example");
        let roots = shared_dirs(&td);
        ensure_profile_provisioned_with("example", &dir, &roots).unwrap();
        let again = ensure_profile_provisioned_with("example", &dir, &roots).unwrap();
        assert_eq!(again.plugins, LinkOutcome::AlreadyLinked);
        assert_eq!(again.projects, LinkOutcome::AlreadyLinked);
        assert_eq!(again.sessions, LinkOutcome::AlreadyLinked);
    }

    /// The three subdir names, in the order [`ProfileDiagnosis`] reports them.
    const AXES: [&str; 3] = ["plugins", "projects", "sessions"];

    /// Assert every prior state of the `sub` subdir is classified independently.
    /// The OTHER TWO subdirs are always linked correctly, so `is_healthy()`
    /// reflects only the axis under test.
    fn assert_diagnoses_each_state(sub: &str) {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let shared_for = |s: &str| match s {
            "plugins" => roots.plugins.clone(),
            "projects" => roots.projects.clone(),
            _ => roots.sessions.clone(),
        };
        for s in AXES {
            fs::create_dir_all(shared_for(s)).unwrap();
        }
        let axis = |d: &ProfileDiagnosis, s: &str| match s {
            "plugins" => d.plugins.clone(),
            "projects" => d.projects.clone(),
            _ => d.sessions.clone(),
        };
        // Link every axis but the one under test, so the verdict isolates it.
        let link_others = |dir: &Path| {
            for other in AXES.iter().filter(|o| **o != sub) {
                std::os::unix::fs::symlink(shared_for(other), dir.join(other)).unwrap();
            }
        };

        // Missing: dir exists, no entry for `sub`.
        let missing = td.path().join(".claude.missing");
        fs::create_dir_all(&missing).unwrap();
        link_others(&missing);
        let d = diagnose_profile_with(&missing, &roots);
        assert!(d.dir_exists);
        assert_eq!(axis(&d, sub), LinkState::Missing);
        assert!(!d.is_healthy());

        // Healthy: correct symlink.
        let ok = td.path().join(".claude.ok");
        fs::create_dir_all(&ok).unwrap();
        std::os::unix::fs::symlink(shared_for(sub), ok.join(sub)).unwrap();
        link_others(&ok);
        let d = diagnose_profile_with(&ok, &roots);
        assert_eq!(axis(&d, sub), LinkState::Ok);
        assert!(d.is_healthy());

        // RealDir: per-profile dir (the cache-miss / split-history /
        // split-peer-registry cause).
        let real = td.path().join(".claude.real");
        fs::create_dir_all(real.join(sub)).unwrap();
        link_others(&real);
        let d = diagnose_profile_with(&real, &roots);
        assert_eq!(axis(&d, sub), LinkState::RealDir);
        assert!(!d.is_healthy());

        // WrongLink: symlink to somewhere else.
        let wrong = td.path().join(".claude.wrong");
        fs::create_dir_all(&wrong).unwrap();
        let elsewhere = td.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, wrong.join(sub)).unwrap();
        link_others(&wrong);
        let d = diagnose_profile_with(&wrong, &roots);
        assert!(matches!(axis(&d, sub), LinkState::WrongLink(_)));
        assert!(!d.is_healthy());

        // Dir absent entirely.
        let gone = td.path().join(".claude.gone");
        let d = diagnose_profile_with(&gone, &roots);
        assert!(!d.dir_exists);
    }

    #[test]
    fn diagnose_classifies_each_state_plugins() {
        assert_diagnoses_each_state("plugins");
    }

    #[test]
    fn diagnose_classifies_each_state_projects() {
        assert_diagnoses_each_state("projects");
    }

    #[test]
    fn diagnose_classifies_each_state_sessions() {
        assert_diagnoses_each_state("sessions");
    }

    #[test]
    fn projects_real_dir_is_unhealthy_even_when_plugins_ok() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.plugins).unwrap();
        fs::create_dir_all(&roots.sessions).unwrap();
        let dir = td.path().join(".claude.real");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&roots.plugins, dir.join("plugins")).unwrap();
        std::os::unix::fs::symlink(&roots.sessions, dir.join("sessions")).unwrap();
        fs::create_dir_all(dir.join("projects")).unwrap();

        let d = diagnose_profile_with(&dir, &roots);
        assert_eq!(d.plugins, LinkState::Ok);
        assert_eq!(d.projects, LinkState::RealDir);
        assert!(
            !d.is_healthy(),
            "a diverged projects dir must not be masked by a healthy plugins link"
        );
    }

    #[test]
    fn diagnose_then_fix_makes_healthy() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.plugins).unwrap();
        fs::create_dir_all(&roots.projects).unwrap();
        fs::create_dir_all(&roots.sessions).unwrap();
        let real = td.path().join(".claude.real");
        for sub in AXES {
            fs::create_dir_all(real.join(sub)).unwrap();
        }

        let before = diagnose_profile_with(&real, &roots);
        assert_eq!(before.plugins, LinkState::RealDir);
        assert_eq!(before.projects, LinkState::RealDir);
        assert_eq!(before.sessions, LinkState::RealDir);
        ensure_profile_provisioned_with("real", &real, &roots).unwrap();
        assert!(
            diagnose_profile_with(&real, &roots).is_healthy(),
            "fix must make the profile healthy"
        );
    }

    #[test]
    fn two_profiles_share_one_ssot() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = td.path().join(".claude.a");
        let b = td.path().join(".claude.b");
        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        ensure_profile_provisioned_with("b", &b, &roots).unwrap();
        // A file written through profile a's link is visible through profile b's,
        // on all three axes.
        fs::write(a.join("plugins").join("shared.json"), b"{}").unwrap();
        assert!(
            b.join("plugins").join("shared.json").exists(),
            "both profiles must see the same plugins SSOT"
        );
        fs::write(a.join("projects").join("transcript.json"), b"{}").unwrap();
        assert!(
            b.join("projects").join("transcript.json").exists(),
            "both profiles must see the same projects SSOT"
        );
        // The one that matters for cross-session messaging: a session that
        // registered while profile a was active is discoverable from profile b.
        fs::write(a.join("sessions").join("4242.json"), b"{}").unwrap();
        assert!(
            b.join("sessions").join("4242.json").exists(),
            "a peer registered under one profile must be visible from the other"
        );
    }

    // ─── the sessions axis: swap first, then drain; never strand an entry ─────

    /// A 64-hex key hash, as Claude Code derives from the socket path.
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A profile dir with a real `sessions/` holding `(name, contents)` entries.
    fn profile_with_sessions(
        td: &tempfile::TempDir,
        name: &str,
        entries: &[(&str, &str)],
    ) -> PathBuf {
        let dir = td.path().join(name);
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (e, body) in entries {
            fs::write(sessions.join(e), body).unwrap();
        }
        dir
    }

    /// Set `path`'s mtime to `secs` after the epoch.
    fn set_mtime(path: &Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// The staging dirs under the shared root.
    fn staging(roots: &SharedRoots) -> Vec<PathBuf> {
        staging_dirs_in(roots.sessions.parent().unwrap())
    }

    fn merged(moved: usize) -> LinkOutcome {
        LinkOutcome::Merged {
            moved,
            leftover: Vec::new(),
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn registry_names_are_classified() {
        use RegistryName::*;
        let key = format!("123.{HASH}.key");
        let tmp = format!("123.{HASH}.key.tmp.9f0a");
        for (name, want) in [
            ("123.json", PidJson),
            (key.as_str(), Key),
            (tmp.as_str(), KeyTmp),
            (".fleetview-heartbeat", Heartbeat),
            ("007x.json", Other),
            (".json", Other),
            ("123.abc.key", Other),
            ("notes.txt", Other),
        ] {
            assert_eq!(RegistryName::classify(name), want, "{name}");
        }
        let upper = format!("123.{}.key", HASH.to_uppercase());
        assert_eq!(RegistryName::classify(&upper), Other);
        let bad_tmp = format!("123.{HASH}.key.tmp.");
        assert_eq!(RegistryName::classify(&bad_tmp), Other);
    }

    #[test]
    fn fresh_sessions_ssot_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let dir = td.path().join(".claude.a");
        ensure_profile_provisioned_with("a", &dir, &roots).unwrap();
        let mode = fs::metadata(&roots.sessions).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "the peer registry must not be world-listable"
        );
    }

    #[test]
    fn existing_sessions_ssot_mode_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.sessions).unwrap();
        fs::set_permissions(&roots.sessions, fs::Permissions::from_mode(0o750)).unwrap();
        ensure_profile_provisioned_with("a", &td.path().join(".claude.a"), &roots).unwrap();
        let mode = fs::metadata(&roots.sessions).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }

    #[test]
    fn real_sessions_dir_is_swapped_and_drained() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        // Profile a registers first; the SSOT then exists with a's entries.
        let key_a = format!("100.{HASH}.key");
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "a"), (&key_a, "ka")]);
        assert_eq!(
            ensure_profile_provisioned_with("a", &a, &roots)
                .unwrap()
                .sessions,
            merged(2),
            "the SSOT is created empty, so a's entries are drained, not seeded"
        );
        // Profile b has its own live entries; they must survive.
        let key_b = format!("200.{HASH}.key");
        let b = profile_with_sessions(&td, ".claude.b", &[("200.json", "b"), (&key_b, "kb")]);

        let report = ensure_profile_provisioned_with("b", &b, &roots).unwrap();
        assert_eq!(report.sessions, merged(2));
        assert!(is_symlink_to(&b.join("sessions"), &roots.sessions));
        assert!(
            !b.join("sessions.bak").exists(),
            "a live registry is never backed up"
        );
        for entry in ["100.json", key_a.as_str(), "200.json", key_b.as_str()] {
            assert!(
                roots.sessions.join(entry).exists(),
                "{entry} must be in the SSOT"
            );
            assert!(a.join("sessions").join(entry).exists(), "{entry} via a");
            assert!(b.join("sessions").join(entry).exists(), "{entry} via b");
        }
        assert!(staging(&roots).is_empty(), "no staging dir may remain");
        assert!(diagnose_profile_with(&b, &roots).is_healthy());
    }

    #[test]
    fn sessions_provisioning_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "a")]);
        assert_eq!(
            ensure_profile_provisioned_with("a", &a, &roots)
                .unwrap()
                .sessions,
            merged(1)
        );
        assert_eq!(
            ensure_profile_provisioned_with("a", &a, &roots)
                .unwrap()
                .sessions,
            LinkOutcome::AlreadyLinked,
            "the second call must be a no-op"
        );
    }

    #[test]
    fn empty_sessions_dir_just_links() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[]);
        assert_eq!(
            ensure_profile_provisioned_with("a", &a, &roots)
                .unwrap()
                .sessions,
            merged(0)
        );
        assert!(is_symlink_to(&a.join("sessions"), &roots.sessions));
        assert!(staging(&roots).is_empty());
    }

    /// Profile b's staged copy of `name` and the SSOT's copy collide; returns
    /// the SSOT's content afterwards. `staged_secs` / `shared_secs` set mtimes.
    fn collide(name: &str, staged: (&str, u64), shared: (&str, u64)) -> (String, LinkOutcome) {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.sessions).unwrap();
        fs::write(roots.sessions.join(name), shared.0).unwrap();
        set_mtime(&roots.sessions.join(name), shared.1);
        let b = profile_with_sessions(&td, ".claude.b", &[(name, staged.0)]);
        set_mtime(&b.join("sessions").join(name), staged.1);

        let report = ensure_profile_provisioned_with("b", &b, &roots).unwrap();
        assert!(
            is_symlink_to(&b.join("sessions"), &roots.sessions),
            "linked either way"
        );
        assert!(
            staging(&roots).is_empty(),
            "a recognised collision leaves no staging"
        );
        (read(&roots.sessions.join(name)), report.sessions)
    }

    #[test]
    fn colliding_pid_record_keeps_the_newer_mtime() {
        let (content, out) = collide("100.json", ("staged", 2_000), ("shared", 1_000));
        assert_eq!(
            content, "staged",
            "a newer staged record replaces the SSOT's"
        );
        assert_eq!(out, merged(1));

        let (content, out) = collide("100.json", ("staged", 1_000), ("shared", 2_000));
        assert_eq!(content, "shared", "an older staged record is dropped");
        assert_eq!(out, merged(0));

        let (content, _) = collide("100.json", ("staged", 1_000), ("shared", 1_000));
        assert_eq!(content, "shared", "an mtime tie keeps the SSOT's copy");
    }

    #[test]
    fn colliding_key_dedupes_or_keeps_the_newer() {
        let key = format!("100.{HASH}.key");
        let (content, _) = collide(&key, ("same", 2_000), ("same", 1_000));
        assert_eq!(content, "same", "identical keys are deduped");

        let (content, _) = collide(&key, ("new", 2_000), ("old", 1_000));
        assert_eq!(content, "new");
        let (content, _) = collide(&key, ("old", 1_000), ("new", 2_000));
        assert_eq!(content, "new");
    }

    #[test]
    fn colliding_key_tmp_is_dropped() {
        let tmp = format!("100.{HASH}.key.tmp.ab12");
        let (content, _) = collide(&tmp, ("staged", 2_000), ("shared", 1_000));
        assert_eq!(
            content, "shared",
            "a tmp file is garbage; the staged one goes"
        );
    }

    #[test]
    fn heartbeat_in_both_never_blocks_the_migration() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.sessions).unwrap();
        fs::write(roots.sessions.join(".fleetview-heartbeat"), "1").unwrap();
        let b = profile_with_sessions(
            &td,
            ".claude.b",
            &[(".fleetview-heartbeat", "2"), ("200.json", "b")],
        );

        let report = ensure_profile_provisioned_with("b", &b, &roots).unwrap();
        assert!(
            matches!(report.sessions, LinkOutcome::Merged { ref leftover, .. } if leftover.is_empty())
        );
        assert!(is_symlink_to(&b.join("sessions"), &roots.sessions));
        assert!(roots.sessions.join("200.json").exists());
        assert!(staging(&roots).is_empty());
        // And it stays that way: the next run does not trip over it either.
        assert_eq!(
            ensure_profile_provisioned_with("b", &b, &roots)
                .unwrap()
                .sessions,
            LinkOutcome::AlreadyLinked
        );
    }

    #[test]
    fn unrecognised_collision_and_subdir_stay_in_reported_staging() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.sessions).unwrap();
        fs::write(roots.sessions.join("notes.txt"), "shared").unwrap();
        let b = profile_with_sessions(
            &td,
            ".claude.b",
            &[
                ("notes.txt", "staged"),
                ("200.json", "b"),
                ("future.dat", "new"),
            ],
        );
        fs::create_dir_all(b.join("sessions").join("unexpected")).unwrap();

        let report = ensure_profile_provisioned_with("b", &b, &roots).unwrap();
        let LinkOutcome::Merged { moved, leftover } = report.sessions else {
            panic!("expected Merged, got {:?}", report.sessions);
        };
        // 200.json and the unknown-but-uncolliding future.dat moved across.
        assert_eq!(moved, 2);
        assert!(roots.sessions.join("200.json").exists());
        assert_eq!(read(&roots.sessions.join("future.dat")), "new");
        assert!(
            is_symlink_to(&b.join("sessions"), &roots.sessions),
            "still linked"
        );
        // The unknown collision and the subdir are kept, not deleted, and reported.
        assert_eq!(leftover.len(), 1);
        assert_eq!(read(&leftover[0].join("notes.txt")), "staged");
        assert!(leftover[0].join("unexpected").is_dir());
        assert_eq!(read(&roots.sessions.join("notes.txt")), "shared");
        assert_eq!(staging(&roots), leftover);
        // Doctor's view: the profile is healthy; the staging dir is listed separately.
        assert!(diagnose_profile_with(&b, &roots).is_healthy());
    }

    #[test]
    fn leftover_staging_is_drained_on_an_already_linked_profile() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = td.path().join(".claude.a");
        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        // A run that crashed between the swap and the drain.
        let stage = roots
            .sessions
            .parent()
            .unwrap()
            .join(".sessions-staging.x.1.0");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("300.json"), "live").unwrap();

        let report = ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        assert_eq!(report.sessions, merged(1));
        assert_eq!(read(&a.join("sessions").join("300.json")), "live");
        assert!(!stage.exists(), "the drained staging dir is removed");
    }

    #[test]
    fn a_first_link_still_reports_what_it_drained() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        // A profile with no `sessions` entry at all: the link itself is a plain
        // `Created`, but this run also recovers what an interrupted one left.
        let a = td.path().join(".claude.a");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(roots.sessions.parent().unwrap()).unwrap();
        ensure_private_dir(&roots.sessions).unwrap();
        fs::write(roots.sessions.join("notes.txt"), "shared").unwrap();
        let stage = roots
            .sessions
            .parent()
            .unwrap()
            .join(".sessions-staging.x.1.0");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("300.json"), "live").unwrap();
        fs::write(stage.join("notes.txt"), "staged").unwrap();

        let report = ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        assert_eq!(
            report.sessions,
            LinkOutcome::Merged {
                moved: 1,
                leftover: vec![stage.clone()],
            },
            "a `Created` link must not swallow the drain's result"
        );
        assert_eq!(read(&roots.sessions.join("300.json")), "live");
        assert_eq!(read(&roots.sessions.join("notes.txt")), "shared");
        assert_eq!(read(&stage.join("notes.txt")), "staged");
        assert!(is_symlink_to(&a.join("sessions"), &roots.sessions));
    }

    #[test]
    fn symlink_named_like_staging_is_never_drained() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = td.path().join(".claude.a");
        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        fs::write(roots.sessions.join("100.json"), "live").unwrap();
        // Were this followed, every entry would "collide" with itself.
        let fake = roots
            .sessions
            .parent()
            .unwrap()
            .join(".sessions-staging.x.1.0");
        std::os::unix::fs::symlink(&roots.sessions, &fake).unwrap();

        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        assert_eq!(read(&roots.sessions.join("100.json")), "live");
    }

    #[test]
    fn dir_recreated_in_the_swap_gap_is_staged_too() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "a")]);
        let link = a.join("sessions");
        // A registering session's `mkdir -p` + write lands between the
        // rename-aside and the symlink, once.
        let fired = std::cell::Cell::new(false);
        let gap = || {
            if !fired.replace(true) {
                fs::create_dir_all(&link).unwrap();
                fs::write(link.join("300.json"), "new").unwrap();
            }
        };

        let out = link_sessions_with(
            &link,
            &roots.sessions,
            roots.sessions.parent().unwrap(),
            &gap,
        )
        .unwrap();
        assert_eq!(out, merged(2));
        assert!(is_symlink_to(&link, &roots.sessions));
        assert_eq!(read(&roots.sessions.join("100.json")), "a");
        assert_eq!(read(&roots.sessions.join("300.json")), "new");
        assert!(staging(&roots).is_empty());
    }

    #[test]
    fn dir_recreated_every_time_gives_up_without_losing_entries() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "a")]);
        let link = a.join("sessions");
        let n = std::cell::Cell::new(0u32);
        let gap = || {
            n.set(n.get() + 1);
            fs::create_dir_all(&link).unwrap();
            fs::write(link.join(format!("{}.json", 300 + n.get())), "new").unwrap();
        };

        let err = link_sessions_with(
            &link,
            &roots.sessions,
            roots.sessions.parent().unwrap(),
            &gap,
        )
        .unwrap_err();
        assert!(err.to_string().contains("gave up"), "{err}");
        assert_eq!(n.get() as usize, SWAP_ATTEMPTS);
        // Everything staged was still carried into the SSOT; only the last
        // recreation remains a real dir, which `doctor` keeps reporting.
        assert!(roots.sessions.join("100.json").exists());
        assert!(roots.sessions.join("301.json").exists());
        assert!(staging(&roots).is_empty());
        assert_eq!(
            diagnose_profile_with(&a, &roots).sessions,
            LinkState::RealDir
        );
    }

    #[test]
    fn a_failed_link_after_the_swap_still_drains_the_staged_entries() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "live")]);
        let link = a.join("sessions");
        // Make the symlink(2) that follows the rename-aside fail with something
        // other than EEXIST, the way a read-only or full filesystem would.
        // Removing the profile dir (empty now that `sessions` has been renamed
        // aside) does it for any uid, unlike a permission bit root ignores, and
        // it defeats the restore too, leaving the drain as the only way back.
        let profile = a.clone();
        let gap = move || {
            fs::remove_dir_all(&profile).unwrap();
        };

        let err = link_sessions_with(
            &link,
            &roots.sessions,
            roots.sessions.parent().unwrap(),
            &gap,
        )
        .unwrap_err();

        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "a hard failure, not the gap race the swap loop retries"
        );
        assert_eq!(
            read(&roots.sessions.join("100.json")),
            "live",
            "a live record must reach the SSOT even when the link could not be made"
        );
        assert!(
            staging(&roots).is_empty(),
            "an error must not park entries in staging until the next run"
        );
    }

    #[test]
    fn restore_staged_puts_the_directory_back() {
        let td = tempfile::tempdir().unwrap();
        let stage = td.path().join(".sessions-staging.a.1.0");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("100.json"), "live").unwrap();
        let link = td.path().join(".claude.a").join("sessions");
        fs::create_dir_all(link.parent().unwrap()).unwrap();

        let mut staged = vec![stage.clone()];
        restore_staged(&mut staged, &link);
        assert!(staged.is_empty(), "a restored dir is no longer staged");
        assert_eq!(read(&link.join("100.json")), "live");
        assert!(!stage.exists());

        // A restore that cannot happen keeps the path staged, so the caller
        // drains it into the SSOT instead of dropping it.
        let mut staged = vec![td.path().join("gone")];
        restore_staged(&mut staged, &link);
        assert_eq!(staged.len(), 1);
    }

    #[test]
    fn a_concurrent_link_in_the_gap_still_drains_what_this_run_staged() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "a")]);
        let link = a.join("sessions");
        // A concurrent csm links the profile in the gap between our
        // rename-aside and our symlink, so our own link attempt loses.
        let (l, s) = (link.clone(), roots.sessions.clone());
        let fired = std::cell::Cell::new(false);
        let gap = || {
            if !fired.replace(true) {
                fs::create_dir_all(&s).unwrap();
                std::os::unix::fs::symlink(&s, &l).unwrap();
            }
        };

        let out = link_sessions_with(
            &link,
            &roots.sessions,
            roots.sessions.parent().unwrap(),
            &gap,
        )
        .unwrap();
        assert_eq!(out, merged(1), "the entry we staged is still drained");
        assert!(is_symlink_to(&link, &roots.sessions));
        assert_eq!(read(&roots.sessions.join("100.json")), "a");
        assert!(staging(&roots).is_empty());
    }

    #[test]
    fn leftover_staging_is_drained_without_touching_any_profile() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = td.path().join(".claude.a");
        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        let parent = roots.sessions.parent().unwrap();
        // What a run killed between the swap and the drain leaves behind …
        let stage = parent.join(".sessions-staging.a.1.0");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("300.json"), "live").unwrap();
        // … next to one holding an entry the collision policy will not move.
        let stuck = parent.join(".sessions-staging.a.1.1");
        fs::create_dir_all(&stuck).unwrap();
        fs::write(roots.sessions.join("notes.txt"), "shared").unwrap();
        fs::write(stuck.join("notes.txt"), "staged").unwrap();

        // `doctor --fix` provisions nothing here — the profile is already
        // healthy — so the drain is what has to clear this.
        assert!(diagnose_profile_with(&a, &roots).is_healthy());
        assert_eq!(drain_staging_under(&roots.sessions), vec![stuck.clone()]);
        assert_eq!(read(&a.join("sessions").join("300.json")), "live");
        assert!(!stage.exists(), "the mergeable staging dir is gone");
        assert_eq!(
            read(&stuck.join("notes.txt")),
            "staged",
            "nothing is deleted"
        );
        assert!(
            is_symlink_to(&a.join("sessions"), &roots.sessions),
            "no profile's own entry is touched"
        );
    }

    #[test]
    fn concurrent_writer_during_the_swap_loses_nothing() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        fs::create_dir_all(&roots.sessions).unwrap();
        let a = profile_with_sessions(&td, ".claude.a", &[("100.json", "0")]);
        let record = a.join("sessions").join("100.json");
        let stop = Arc::new(AtomicBool::new(false));

        // A live session's read-modify-write of its own record, tolerating the
        // record being briefly unreachable, as Claude Code's update does.
        let writer = {
            let stop = Arc::clone(&stop);
            let record = record.clone();
            std::thread::spawn(move || {
                for i in 0..200_000u32 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if fs::read(&record).is_ok() {
                        let _ = fs::write(&record, i.to_string());
                    }
                }
            })
        };
        let result = ensure_profile_provisioned_with("a", &a, &roots);
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        result.unwrap();
        assert!(is_symlink_to(&a.join("sessions"), &roots.sessions));
        assert!(roots.sessions.join("100.json").is_file());
        assert!(staging(&roots).is_empty());
    }

    #[test]
    fn concurrent_provisions_of_one_profile_both_succeed() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let entries: Vec<(String, String)> = (0..50)
            .map(|i| (format!("{}.json", 100 + i), i.to_string()))
            .collect();
        let borrowed: Vec<(&str, &str)> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_str()))
            .collect();
        let a = profile_with_sessions(&td, ".claude.a", &borrowed);
        // Only the sessions axis races here; plugins/projects are already linked.
        for (sub, shared) in [("plugins", &roots.plugins), ("projects", &roots.projects)] {
            fs::create_dir_all(shared).unwrap();
            std::os::unix::fs::symlink(shared, a.join(sub)).unwrap();
        }

        let results: Vec<io::Result<ProvisionReport>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|_| s.spawn(|| ensure_profile_provisioned_with("a", &a, &roots)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in results {
            r.unwrap();
        }
        assert!(is_symlink_to(&a.join("sessions"), &roots.sessions));
        for (name, body) in &entries {
            assert_eq!(&read(&roots.sessions.join(name)), body, "{name} lost");
        }
        assert!(staging(&roots).is_empty());
    }

    #[test]
    fn dangling_link_is_diagnosed_and_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let a = td.path().join(".claude.a");
        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        fs::remove_dir(&roots.sessions).unwrap();
        fs::remove_dir(&roots.plugins).unwrap();

        let d = diagnose_profile_with(&a, &roots);
        assert_eq!(d.sessions, LinkState::Dangling);
        assert_eq!(d.plugins, LinkState::Dangling);
        assert!(!d.is_healthy(), "a dangling link must not read as healthy");

        ensure_profile_provisioned_with("a", &a, &roots).unwrap();
        assert!(diagnose_profile_with(&a, &roots).is_healthy());
        let mode = fs::metadata(&roots.sessions).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            roots.plugins.is_dir(),
            "every axis recreates a missing target"
        );
    }

    #[test]
    fn sessions_wrong_link_is_repointed() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let dir = td.path().join(".claude.a");
        fs::create_dir_all(&dir).unwrap();
        let elsewhere = td.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("sessions")).unwrap();

        let report = ensure_profile_provisioned_with("a", &dir, &roots).unwrap();
        assert_eq!(report.sessions, LinkOutcome::Created);
        assert!(is_symlink_to(&dir.join("sessions"), &roots.sessions));
    }

    #[test]
    fn sessions_regular_file_is_backed_up() {
        let td = tempfile::tempdir().unwrap();
        let roots = shared_dirs(&td);
        let dir = td.path().join(".claude.a");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("sessions"), "stray").unwrap();

        let report = ensure_profile_provisioned_with("a", &dir, &roots).unwrap();
        let LinkOutcome::BackedUp(backup) = report.sessions else {
            panic!("expected BackedUp, got {:?}", report.sessions);
        };
        assert_eq!(read(&backup), "stray");
        assert!(is_symlink_to(&dir.join("sessions"), &roots.sessions));
    }
}

/// [`provisioning_allowed`] unit tests. Platform-independent (the predicate
/// touches neither the filesystem nor the environment), unlike `mod tests`
/// above, which exercises the actual symlink machinery and is unix-only.
#[cfg(test)]
mod gate_tests {
    use super::*;
    use std::collections::HashMap;

    fn registry(pairs: &[(&str, &str)]) -> account::ProfileMap {
        account::ProfileMap(
            pairs
                .iter()
                .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
                .collect::<HashMap<_, _>>(),
        )
    }

    const HOME: &str = "/Users/example";
    const OTHER_HOME: &str = "/Users/other";

    #[test]
    fn registered_dir_inside_home_is_allowed() {
        let reg = registry(&[("work", "/Users/example/.claude.work")]);
        assert!(provisioning_allowed(
            Some(&reg),
            Some(Path::new(HOME)),
            Path::new("/Users/example/.claude.work")
        ));
    }

    #[test]
    fn conventional_shape_dir_inside_home_is_allowed() {
        let reg = registry(&[]);
        assert!(provisioning_allowed(
            Some(&reg),
            Some(Path::new(HOME)),
            Path::new("/Users/example/.claude.home")
        ));
    }

    /// The incident shape: an unregistered profile dir sits under one home,
    /// while the resolved home (and therefore the shared SSOT) is a
    /// DIFFERENT one — e.g. `$CLAUDE_CONFIG_DIR` naming a real-home profile
    /// while `$HOME` points at a sandbox. Must be refused.
    #[test]
    fn unregistered_dir_under_a_different_home_is_refused() {
        let reg = registry(&[]);
        assert!(!provisioning_allowed(
            Some(&reg),
            Some(Path::new(OTHER_HOME)),
            Path::new("/Users/example/.claude.work")
        ));
    }

    /// A first-boot box has no `profiles.json` yet (empty registry), but
    /// `paths::synthesize_profile_dir` still invents `<home>/.claude.<name>`
    /// for an unregistered name — that must keep working.
    #[test]
    fn empty_registry_with_conventional_dir_is_allowed() {
        let reg = registry(&[]);
        assert!(provisioning_allowed(
            Some(&reg),
            Some(Path::new(HOME)),
            Path::new("/Users/example/.claude.new")
        ));
    }

    /// `account::ProfileMap::load()` failing (corrupt/unreadable
    /// `profiles.json`) must be a refusal even for an otherwise-conventional
    /// dir — an unreadable registry cannot confirm the dir is NOT registered
    /// somewhere unconventional, so it never gets the benefit of the doubt.
    #[test]
    fn unreadable_registry_is_refused_even_for_a_conventional_dir() {
        assert!(!provisioning_allowed(
            None,
            Some(Path::new(HOME)),
            Path::new("/Users/example/.claude.home")
        ));
    }

    /// No resolvable home at all — only the registry can authorise a dir.
    #[test]
    fn without_a_home_only_the_registry_authorises() {
        let reg = registry(&[("work", "/Users/example/.claude.work")]);
        assert!(provisioning_allowed(
            Some(&reg),
            None,
            Path::new("/Users/example/.claude.work")
        ));
        assert!(!provisioning_allowed(
            Some(&reg),
            None,
            Path::new("/Users/example/.claude.home")
        ));
    }
}
