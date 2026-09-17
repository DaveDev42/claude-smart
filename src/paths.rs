//! Paths SSOT — every `$SMART_DIR`-relative filename in one place.
//!
//! Rule: **no hardcoded path strings outside this module**. Every caller that
//! needs a file under `smart_dir()` uses the constructors here.
//!
//! `$SMART_DIR` = `$HOME/.claude.shared/smart` (POSIX) or
//!               `%USERPROFILE%\.claude.shared\smart` (Windows).
//! [`home_dir`] resolves the current user's home directory cross-platform.

use std::io;
use std::path::{Path, PathBuf};

/// The one home-dir resolver every path constructor in this module (and
/// `usage::local::creds`) uses.
///
/// Production body is exactly `dirs::home_dir()`. Under test, `dirs::home_dir()`
/// itself is not fixture-friendly on Windows: it calls
/// `SHGetKnownFolderPath(FOLDERID_Profile)` unconditionally and never reads
/// `HOME`/`USERPROFILE`, so a fixture that sets those env vars gets zero
/// isolation there. Tests instead override this function's return value
/// directly via a thread-local set with [`crate::testenv::set_test_home`].
#[cfg(not(test))]
pub(crate) fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

#[cfg(test)]
pub(crate) fn home_dir() -> Option<PathBuf> {
    crate::testenv::test_home().or_else(dirs::home_dir)
}

/// Return the smart state directory, creating it if it does not yet exist.
///
/// The "lazy create" contract: callers that only *read* state (e.g. the TTY-gate
/// check that peeks at `.usage-cache.json`) should call `smart_dir_no_create()`
/// to avoid spurious dir creation in non-interactive contexts. Writers call this.
pub fn smart_dir() -> io::Result<PathBuf> {
    let dir = smart_dir_no_create();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Return the smart state directory path without creating it.
pub fn smart_dir_no_create() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.shared")
        .join("smart")
}

/// `~/.claude.<name>` — the conventional profile config dir for a profile that
/// has no explicit entry in `ProfileMap` (toss machines, first-boot before the
/// registry is populated, or a bare token passed straight through). Shares
/// only the path-string construction: `ProfileMap` stays the sole registry
/// *authority* over which profiles exist and where they actually live.
pub fn synthesize_profile_dir(name: &str) -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!(".claude.{name}"))
}

/// `~/.config/claude-as/` — the profile-switch contract shared with the `cas`
/// shell shims. `profiles_json()` and `cas::default_state_file()` each join
/// their own leaf onto this.
pub fn claude_as_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("claude-as")
}

// ─── session-level paths ──────────────────────────────────────────────────────

/// `<smart_dir>/<sid>.json` — sidecar (mode/effort/model/cwd/profile/hop).
pub fn sidecar(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.json"))
}

/// `<smart_dir>/<sid>.relaunch` — limit-switch handoff sentinel.
pub fn relaunch(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.relaunch"))
}

/// `<smart_dir>/<sid>.pid` — PID + born epoch written by the foreground supervisor.
pub fn pid_file(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.pid"))
}

/// `<smart_dir>/<sid>.stop` — Windows-only IPC flag: hook writes, supervisor polls.
/// Presence signals "stop requested"; content is unused.
/// (POSIX uses SIGTERM instead, so this is dead on unix builds.)
#[cfg_attr(unix, allow(dead_code))]
pub fn stop_flag(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.stop"))
}

/// `<smart_dir>/<sid>.switched` — anti-loop guard marker (epoch, existence-only).
pub fn switched(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.switched"))
}

/// `<smart_dir>/<sid>.detected` — notify-dedup marker (epoch, existence-only).
/// Pruned after 7 days.
pub fn detected(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.detected"))
}

// ─── global state paths ───────────────────────────────────────────────────────

/// `<smart_dir>/.usage-cache.json` — positive TTL usage cache (60 s by mtime).
pub fn usage_cache() -> PathBuf {
    smart_dir_no_create().join(".usage-cache.json")
}

/// `<smart_dir>/.usage-fetch-failed` — negative cooldown marker (bare epoch).
pub fn fetch_failed() -> PathBuf {
    smart_dir_no_create().join(".usage-fetch-failed")
}

/// `<smart_dir>/.last-switch` — machine-wide cooldown marker (bare epoch, content
/// is authoritative — NOT an mtime lock).
pub fn last_switch() -> PathBuf {
    smart_dir_no_create().join(".last-switch")
}

/// `<smart_dir>/titles.tsv` — session-name alias index (`title \t sid \t mtime`).
pub fn titles_tsv() -> PathBuf {
    smart_dir_no_create().join("titles.tsv")
}

/// `~/.config/claude-as/profiles.json` — cross-platform profile→dir map.
/// The registry is optional: when the file is absent, csm falls back to the
/// current `CLAUDE_CONFIG_DIR` and disables the switch/pick features.
pub fn profiles_json() -> PathBuf {
    claude_as_dir().join("profiles.json")
}

/// `~/.config/claude-smart/config.json` — csm's OWN global config (drop-in
/// launch command + future settings). Distinct from `~/.config/claude-as/`,
/// which is the profile-switch contract shared with the `cas` shell shims;
/// this is csm's own runtime config, not part of that contract.
pub fn config_json() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("claude-smart")
        .join("config.json")
}

// ─── local usage collection (src/usage/local/) ─────────────────────────────────

/// `<smart_dir>/usage/` — per-profile local usage-collection records
/// (`usage::local::store`). One JSON file per profile so the statusline
/// recorder's frequent writes to the *active* profile's file never race the
/// fetch-all writer's writes to every OTHER profile's file — only same-profile
/// writers can collide, and that collision is an accepted last-writer-wins
/// (see the local-collection design spec, "스토어 레코드").
pub fn usage_store_dir() -> PathBuf {
    smart_dir_no_create().join("usage")
}

/// `<smart_dir>/usage/<profile>.json` — one profile's local usage record.
///
/// `profile` MUST already be validated via
/// [`crate::account::profiles::ProfileMap::is_valid_name`] before it reaches
/// here — like every other constructor in this module, this function is a
/// pure path builder with no sanitization of its own. A profile name pulled
/// from an external source (statusline stdin's `CLAUDE_CONFIG_DIR` reverse
/// lookup) is the caller's responsibility to gate; an unchecked name could
/// otherwise escape the `usage/` directory via `..` or a path separator.
pub fn usage_store(profile: &str) -> PathBuf {
    usage_store_dir().join(format!("{profile}.json"))
}

// ─── scan index path ──────────────────────────────────────────────────────────

/// `<smart_dir>/scan-meta-v2.<enc>.tsv` — per-project-dir incremental scan index.
/// The `v2` prefix ensures the Rust binary's index never collides with the old
/// zsh `scan-meta.<enc>.tsv` (whose format differs slightly). Unknown/old index
/// → treat as absent → full reindex.
pub fn scan_index_for(project_dir: &Path) -> PathBuf {
    let dir_name = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("_unknown");
    smart_dir_no_create().join(format!("scan-meta-v2.{dir_name}.tsv"))
}

/// `$HOME/.claude.shared` — the shared config root. The smart state dir, the
/// cross-profile plugin SSOT, and the cross-profile transcript SSOT all live
/// under here: each profile dir's `plugins` and `projects` are symlinked here
/// (see [`crate::provision::ensure_profile_provisioned`]) so every profile
/// sees one marketplace cache (avoids the `cache-miss` a per-`CLAUDE_CONFIG_DIR`
/// plugin dir causes on switch) and one session history regardless of which
/// profile is active.
pub fn shared_base_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.shared")
}

/// `$HOME/.claude` — Claude Code's default config dir, i.e. where a process that
/// ignores `CLAUDE_CONFIG_DIR` reads and writes. csm keeps it as a compatibility
/// shim: its `projects` entry links to [`session_base_dir`] so tools that
/// hardcode this path still see every profile's transcripts. See
/// [`crate::homeguard`]. Unused off unix, where that link is provisioned
/// OS-side (mirrors `provision`'s platform split).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn home_claude_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// `$HOME/.claude.shared/projects` — the single source of truth for Claude Code
/// session transcripts shared across every profile. Each profile dir's
/// `projects` is symlinked here so `csm`'s own session scanner/alias index
/// (and any other reader of `<CLAUDE_CONFIG_DIR>/projects`) sees every
/// profile's history no matter which `CLAUDE_CONFIG_DIR` is active. See
/// [`crate::provision::ensure_profile_provisioned`].
pub fn session_base_dir() -> PathBuf {
    shared_base_dir().join("projects")
}

/// `$HOME/.claude.shared/plugins` — the single source of truth for Claude Code
/// plugins/marketplaces shared across every profile. Each profile dir's
/// `plugins` is symlinked here so the marketplace cache index stays consistent
/// no matter which `CLAUDE_CONFIG_DIR` is active. See
/// [`crate::provision::ensure_profile_provisioned`].
pub fn shared_plugins_dir() -> PathBuf {
    shared_base_dir().join("plugins")
}

// ─── cwd encoding ─────────────────────────────────────────────────────────────
//
// Claude Code encodes the cwd into the `projects/` subdir name. The rule (per
// the official CC docs + empirical evidence) is a naive per-character
// substitution over the raw native path string: **every non-alphanumeric
// character (`[^A-Za-z0-9]`) → `-`**. No normalization, no run-collapsing.
//
//   POSIX:   /Users/example/Projects/github.com/foo
//            → -Users-example-Projects-github-com-foo
//   Windows: C:\Users\example\Projects\github.com\foo
//            → C--Users-example-Projects-github-com-foo
//            (note `C:\` → `C--`: colon → `-` AND backslash → `-`, two dashes)
//
// A *legacy* variant also exists: older CC versions wrote a directory name
// where only `/` was replaced (`.` and other chars preserved). Both dirs can
// coexist on a machine, so `encode_cwd` returns both and the caller unions all
// that exist on disk (`session_dirs_for`), deduplicating identical results (a
// cwd whose every char is alphanumeric or `/` produces the same string from
// both variants). On Windows the legacy `/`-only string still contains `\`/`:`,
// so that directory never exists on disk → a harmless dead union member.
//
// NOTE (ASCII vs Unicode): we use ASCII `is_ascii_alphanumeric`, matching CC's
// ASCII character class — a non-ASCII folder char (e.g. a Korean letter) is
// non-alphanumeric here and becomes `-`. If a future empirical check shows CC
// preserves Unicode letters, switch `current` to `char::is_alphanumeric`.

/// Return `(current, legacy)` encoded forms of `path`.
///
/// - `current`: every non-alphanumeric char (`[^A-Za-z0-9]`) → `-`
///   (mirrors what Claude Code writes to `projects/<enc>` today).
/// - `legacy`:  every `/` → `-` only (historical CC format; other chars kept).
///
/// When every char of `path` is alphanumeric or `/`, `current == legacy`.
/// Callers must dedup.
pub fn encode_cwd(path: &Path) -> (String, String) {
    let s = path.to_string_lossy();

    // current: replace every non-alphanumeric character with '-'.
    // Mirrors Claude Code's own cwd→projects-dir rule: a naive per-character
    // substitution of `[^A-Za-z0-9]` → `-` over the raw native path string
    // (so a Windows `C:\…` prefix becomes `C--…`: colon → `-` AND backslash → `-`).
    // ASCII-only `is_ascii_alphanumeric` is intentional — CC's rule is an ASCII
    // character class, so non-ASCII path chars are replaced too (see module note).
    let current: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // legacy: replace only '/' with '-' (historical CC format; '.' preserved).
    let legacy: String = s.chars().map(|c| if c == '/' { '-' } else { c }).collect();

    (current, legacy)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Helper: assert encode_cwd returns the expected (current, legacy) pair.
    fn check(raw: &str, expected_current: &str, expected_legacy: &str) {
        let (cur, leg) = encode_cwd(Path::new(raw));
        assert_eq!(
            cur, expected_current,
            "current encoding mismatch for {raw:?}"
        );
        assert_eq!(leg, expected_legacy, "legacy encoding mismatch for {raw:?}");
    }

    #[test]
    fn encode_cwd_home_github_path() {
        // /Users/example/Projects/github.com/some-project
        // current: / and . → -  →  github.com → github-com
        // legacy:  / → - only   →  github.com stays as github.com (dot preserved)
        check(
            "/Users/example/Projects/github.com/some-project",
            "-Users-example-Projects-github-com-some-project",
            "-Users-example-Projects-github.com-some-project",
        );
    }

    #[test]
    fn encode_cwd_path_with_dots_in_segment() {
        // /Users/example/Projects/github.com/some.repo
        // current: dots in "github.com" and "some.repo" → dashes
        // legacy:  dots preserved; only slashes → dashes
        check(
            "/Users/example/Projects/github.com/some.repo",
            "-Users-example-Projects-github-com-some-repo",
            "-Users-example-Projects-github.com-some.repo",
        );
    }

    #[test]
    fn encode_cwd_no_dots() {
        // A path with no dots → current == legacy
        check("/tmp/myproject", "-tmp-myproject", "-tmp-myproject");
    }

    #[test]
    fn encode_cwd_root() {
        check("/", "-", "-");
    }

    #[test]
    fn encode_cwd_multiple_dots() {
        // /home/you/a.b.c/d.e
        check(
            "/home/you/a.b.c/d.e",
            "-home-you-a-b-c-d-e",
            "-home-you-a.b.c-d.e",
        );
    }

    #[test]
    fn encode_cwd_current_legacy_differ_when_dots_present() {
        let (cur, leg) = encode_cwd(Path::new("/foo/bar.baz"));
        // current replaces the dot
        assert!(
            cur.contains("bar-baz"),
            "current should replace dots: {cur}"
        );
        // legacy preserves the dot
        assert!(leg.contains("bar.baz"), "legacy should keep dots: {leg}");
    }

    #[test]
    fn encode_cwd_identical_when_no_dots() {
        let (cur, leg) = encode_cwd(Path::new("/foo/bar/baz"));
        assert_eq!(cur, leg);
    }

    #[test]
    fn encode_cwd_windows_path() {
        // Windows cwd. CC replaces every non-alphanumeric char with '-', so the
        // `C:\` drive prefix becomes `C--` (colon → '-' AND backslash → '-').
        // legacy replaces only '/', so a backslash path is left fully intact —
        // that dir never exists on disk → harmless dead union member.
        //
        // Cross-platform note: `Path::new(r"C:\Users\example\...")` on macOS/Linux
        // treats '\' as an ordinary path char, so `to_string_lossy()` round-trips
        // the backslashes verbatim and this assertion holds on the dev machine too.
        check(
            r"C:\Users\example\Projects\github.com\magicmoment",
            "C--Users-example-Projects-github-com-magicmoment",
            r"C:\Users\example\Projects\github.com\magicmoment",
        );
    }

    #[test]
    fn encode_cwd_space_and_underscore_broaden() {
        // Locks the broad `[^A-Za-z0-9]` rule: spaces and underscores (neither '/'
        // nor '.') must become '-' in the current encoding. legacy keeps them.
        check(
            "/Users/example/My Project/some_repo",
            "-Users-example-My-Project-some-repo",
            "-Users-example-My Project-some_repo",
        );
    }

    #[test]
    fn smart_dir_no_create_is_under_home() {
        let d = smart_dir_no_create();
        // Must contain .claude.shared/smart somewhere in the path
        let s = d.to_string_lossy();
        assert!(
            s.contains(".claude.shared"),
            "smart_dir should be under .claude.shared, got: {s}"
        );
        assert!(
            s.ends_with("smart"),
            "smart_dir should end with 'smart', got: {s}"
        );
    }

    #[test]
    fn home_claude_dir_is_the_bare_default_home() {
        let d = home_claude_dir();
        assert_eq!(d.file_name().and_then(|n| n.to_str()), Some(".claude"));
        let s = d.to_string_lossy();
        assert!(
            !s.contains(".claude."),
            "the default home must not sit in the profile namespace: {s}"
        );
    }

    #[test]
    fn profiles_json_is_under_config() {
        let p = profiles_json();
        let s = p.to_string_lossy();
        assert!(
            s.contains(".config"),
            "profiles_json not under .config: {s}"
        );
        assert!(
            s.contains("claude-as"),
            "profiles_json not under claude-as: {s}"
        );
    }

    #[test]
    fn config_json_is_under_claude_smart() {
        let p = config_json();
        let s = p.to_string_lossy();
        assert!(s.contains(".config"), "config_json not under .config: {s}");
        assert!(
            s.contains("claude-smart"),
            "config_json not under claude-smart: {s}"
        );
        assert!(
            !s.contains("claude-as"),
            "config_json must NOT be under the claude-as profile contract: {s}"
        );
        assert!(
            s.ends_with("config.json"),
            "config_json must end with config.json: {s}"
        );
    }

    #[test]
    fn usage_store_dir_is_under_smart_dir() {
        let d = usage_store_dir();
        let s = d.to_string_lossy();
        assert!(
            s.contains("smart"),
            "usage_store_dir should be under smart_dir: {s}"
        );
        assert!(
            s.ends_with("usage"),
            "usage_store_dir should end with 'usage': {s}"
        );
    }

    #[test]
    fn usage_store_path_includes_profile_name() {
        let p = usage_store("home");
        let s = p.to_string_lossy();
        assert!(s.ends_with("home.json"), "got: {s}");
        assert!(
            s.contains(&*usage_store_dir().to_string_lossy()),
            "usage_store(profile) should live under usage_store_dir(): {s}"
        );
    }

    #[test]
    fn path_constructors_use_sid() {
        let sid = "01234567-89ab-cdef-0123-456789abcdef";
        assert!(sidecar(sid).to_string_lossy().contains(sid));
        assert!(relaunch(sid).to_string_lossy().contains(sid));
        assert!(pid_file(sid).to_string_lossy().contains(sid));
        assert!(stop_flag(sid).to_string_lossy().contains(sid));
        assert!(switched(sid).to_string_lossy().contains(sid));
        assert!(detected(sid).to_string_lossy().contains(sid));
    }
}
