//! Profile provisioning — make a `CLAUDE_CONFIG_DIR` satisfy the invariants csm
//! depends on, idempotently.
//!
//! csm switches profiles by pointing `CLAUDE_CONFIG_DIR` at a per-profile dir
//! (`~/.claude.<name>`). Claude Code stores plugins/marketplaces UNDER that dir,
//! so a naked env swap gives each profile its own plugin store — and switching
//! profiles then breaks the marketplace cache (`cache-miss`, "Run
//! /reload-plugins"). Transcripts already escape this by living in the shared
//! root (`~/.claude.shared/projects`); we extend the same philosophy to plugins.
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
//!
//! ## Platform
//! POSIX uses `std::os::unix::fs::symlink`. On Windows, directory symlinks are
//! privilege-gated and the relaunch loop is currently disabled there, so we make
//! provisioning a no-op rather than fail — the dave-environment Ansible side
//! handles the Windows junction. The plugin-sharing logic is unix-only for now.

use std::io;
use std::path::{Path, PathBuf};

use crate::paths;

/// Outcome of provisioning one profile — reported by `doctor`, ignored by the
/// implicit callers (they only care that it didn't error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionReport {
    /// What the `plugins` link step did.
    pub plugins: LinkOutcome,
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
    /// Skipped (non-unix platform — handled by Ansible/junctions instead).
    /// Only constructed on non-unix builds.
    #[cfg_attr(unix, allow(dead_code))]
    Skipped,
}

// ─── diagnosis (read-only; the `doctor` core) ──────────────────────────────────

/// The state of a profile's `plugins` entry relative to the shared SSOT.
/// Read-only classification — `doctor` reports it; `--fix` calls
/// [`ensure_profile_provisioned`] to repair anything not [`Ok`](PluginLinkState::Ok).
///
/// The non-`Ok` variants are only constructed by the unix `diagnose_profile_with`;
/// the non-unix `diagnose_profile` always reports `Ok` (linking is OS-side), so
/// they carry per-variant dead-code allows to stay warning-clean off unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginLinkState {
    /// `plugins` is a symlink to the shared SSOT — healthy.
    Ok,
    /// No `plugins` entry exists yet (will be created on provision).
    #[cfg_attr(not(unix), allow(dead_code))]
    Missing,
    /// `plugins` is a real directory (per-profile store — the cache-miss cause).
    #[cfg_attr(not(unix), allow(dead_code))]
    RealDir,
    /// `plugins` is a symlink, but to the wrong target.
    #[cfg_attr(not(unix), allow(dead_code))]
    WrongLink(PathBuf),
    /// `plugins` is a regular file (or other non-dir) — unexpected.
    #[cfg_attr(not(unix), allow(dead_code))]
    NotADir,
}

impl PluginLinkState {
    /// Is the profile's plugin link already healthy (no action needed)?
    pub fn is_ok(&self) -> bool {
        matches!(self, PluginLinkState::Ok)
    }
}

/// Read-only diagnosis of one profile — the pure core of `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDiagnosis {
    /// The profile dir exists on disk.
    pub dir_exists: bool,
    /// State of the `plugins` → shared SSOT link.
    pub plugins: PluginLinkState,
}

impl ProfileDiagnosis {
    /// Is the profile fully provisioned (nothing for `--fix` to do)?
    pub fn is_healthy(&self) -> bool {
        self.dir_exists && self.plugins.is_ok()
    }
}

/// Diagnose profile `dir` against the production shared-plugins SSOT.
#[cfg(unix)]
pub fn diagnose_profile(dir: &Path) -> ProfileDiagnosis {
    diagnose_profile_with(dir, &paths::shared_plugins_dir())
}

/// Non-unix: plugin linking is delegated to OS-native tooling, so the diagnosis
/// reports only whether the profile dir exists and treats plugins as `Ok`.
#[cfg(not(unix))]
pub fn diagnose_profile(dir: &Path) -> ProfileDiagnosis {
    ProfileDiagnosis {
        dir_exists: dir.is_dir(),
        plugins: PluginLinkState::Ok,
    }
}

/// [`diagnose_profile`] with the SSOT injected (testable seam). Pure: only reads
/// the filesystem, never mutates.
#[cfg(unix)]
pub fn diagnose_profile_with(dir: &Path, shared_plugins: &Path) -> ProfileDiagnosis {
    let dir_exists = dir.is_dir();
    let link = dir.join("plugins");
    let plugins = match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&link) {
            Ok(target) if links_match(&target, &link, shared_plugins) => PluginLinkState::Ok,
            Ok(target) => PluginLinkState::WrongLink(target),
            Err(_) => PluginLinkState::WrongLink(PathBuf::new()),
        },
        Ok(meta) if meta.is_dir() => PluginLinkState::RealDir,
        Ok(_) => PluginLinkState::NotADir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => PluginLinkState::Missing,
        Err(_) => PluginLinkState::NotADir,
    };
    ProfileDiagnosis {
        dir_exists,
        plugins,
    }
}

/// Ensure profile `name` at `dir` satisfies the provisioning invariants.
///
/// Idempotent: safe to call on every switch/launch/register. Returns a report
/// of what each step did (for `doctor`); implicit callers discard it.
///
/// `dir` is the profile's `CLAUDE_CONFIG_DIR` (from the registry, never a
/// literal). `name` is informational (kept for future per-name steps and for
/// error context).
pub fn ensure_profile_provisioned(name: &str, dir: &Path) -> io::Result<ProvisionReport> {
    ensure_profile_provisioned_with(name, dir, &paths::shared_plugins_dir())
}

/// [`ensure_profile_provisioned`] with the shared-plugins SSOT injected — the
/// testable seam (mirrors `ProfileMap::default_name_with`). Production passes
/// `paths::shared_plugins_dir()`; tests pass a tempdir path.
pub fn ensure_profile_provisioned_with(
    name: &str,
    dir: &Path,
    shared_plugins: &Path,
) -> io::Result<ProvisionReport> {
    // 1. The profile dir itself.
    std::fs::create_dir_all(dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("provision[{name}]: create {} failed: {e}", dir.display()),
        )
    })?;

    // 2. plugins → shared SSOT.
    let plugins = link_dir_to_shared(&dir.join("plugins"), shared_plugins)
        .map_err(|e| io::Error::new(e.kind(), format!("provision[{name}]: plugins: {e}")))?;

    Ok(ProvisionReport { plugins })
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
/// - `link` is already a symlink pointing at `shared` → [`LinkOutcome::AlreadyLinked`].
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
        let shared = td.path().join("shared").join("plugins");
        assert!(!dir.exists());

        let report = ensure_profile_provisioned_with("example", &dir, &shared).unwrap();
        assert!(dir.is_dir(), "profile dir must be created");
        assert_eq!(report.plugins, LinkOutcome::Created);
        assert!(is_symlink_to(&dir.join("plugins"), &shared));
    }

    #[test]
    fn ensure_profile_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(".claude.example");
        let shared = td.path().join("shared").join("plugins");
        ensure_profile_provisioned_with("example", &dir, &shared).unwrap();
        let again = ensure_profile_provisioned_with("example", &dir, &shared).unwrap();
        assert_eq!(again.plugins, LinkOutcome::AlreadyLinked);
    }

    #[test]
    fn diagnose_classifies_each_state() {
        let td = tempfile::tempdir().unwrap();
        let shared = td.path().join("shared").join("plugins");
        fs::create_dir_all(&shared).unwrap();

        // Missing: dir exists, no plugins entry.
        let missing = td.path().join(".claude.missing");
        fs::create_dir_all(&missing).unwrap();
        let d = diagnose_profile_with(&missing, &shared);
        assert!(d.dir_exists);
        assert_eq!(d.plugins, PluginLinkState::Missing);
        assert!(!d.is_healthy());

        // Healthy: correct symlink.
        let ok = td.path().join(".claude.ok");
        fs::create_dir_all(&ok).unwrap();
        std::os::unix::fs::symlink(&shared, ok.join("plugins")).unwrap();
        let d = diagnose_profile_with(&ok, &shared);
        assert_eq!(d.plugins, PluginLinkState::Ok);
        assert!(d.is_healthy());

        // RealDir: per-profile plugins dir (the cache-miss cause).
        let real = td.path().join(".claude.real");
        fs::create_dir_all(real.join("plugins")).unwrap();
        let d = diagnose_profile_with(&real, &shared);
        assert_eq!(d.plugins, PluginLinkState::RealDir);
        assert!(!d.is_healthy());

        // WrongLink: symlink to somewhere else.
        let wrong = td.path().join(".claude.wrong");
        fs::create_dir_all(&wrong).unwrap();
        let other = td.path().join("other");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&other, wrong.join("plugins")).unwrap();
        let d = diagnose_profile_with(&wrong, &shared);
        assert!(matches!(d.plugins, PluginLinkState::WrongLink(_)));
        assert!(!d.is_healthy());

        // Dir absent entirely.
        let gone = td.path().join(".claude.gone");
        let d = diagnose_profile_with(&gone, &shared);
        assert!(!d.dir_exists);
    }

    #[test]
    fn diagnose_then_fix_makes_healthy() {
        let td = tempfile::tempdir().unwrap();
        let shared = td.path().join("shared").join("plugins");
        fs::create_dir_all(&shared).unwrap();
        let real = td.path().join(".claude.real");
        fs::create_dir_all(real.join("plugins")).unwrap();

        assert_eq!(
            diagnose_profile_with(&real, &shared).plugins,
            PluginLinkState::RealDir
        );
        ensure_profile_provisioned_with("real", &real, &shared).unwrap();
        assert!(
            diagnose_profile_with(&real, &shared).is_healthy(),
            "fix must make the profile healthy"
        );
    }

    #[test]
    fn two_profiles_share_one_ssot() {
        let td = tempfile::tempdir().unwrap();
        let shared = td.path().join("shared").join("plugins");
        let a = td.path().join(".claude.a");
        let b = td.path().join(".claude.b");
        ensure_profile_provisioned_with("a", &a, &shared).unwrap();
        ensure_profile_provisioned_with("b", &b, &shared).unwrap();
        // A file written through profile a's link is visible through profile b's.
        fs::write(a.join("plugins").join("shared.json"), b"{}").unwrap();
        assert!(
            b.join("plugins").join("shared.json").exists(),
            "both profiles must see the same SSOT"
        );
    }
}
