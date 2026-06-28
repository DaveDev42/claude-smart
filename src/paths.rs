//! Paths SSOT — every `$SMART_DIR`-relative filename in one place.
//!
//! Rule: **no hardcoded path strings outside this module**. Every caller that
//! needs a file under `smart_dir()` uses the constructors here.
//!
//! `$SMART_DIR` = `$HOME/.claude.shared/smart` (POSIX) or
//!               `%USERPROFILE%\.claude.shared\smart` (Windows).
//! `dirs::home_dir()` resolves `$HOME` / `%USERPROFILE%` cross-platform.

use std::io;
use std::path::{Path, PathBuf};

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
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.shared")
        .join("smart")
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

/// `<smart_dir>/bin/claude-smart-helper.sh` — legacy deployed helper path.
/// Used only for the playbook cleanup (`state: absent`). Not written by the binary.
/// Kept as the SSOT anchor for the not-yet-implemented Ansible cleanup task
/// (rust-port spec §line 447); no Rust caller reads it today.
#[allow(dead_code)]
pub fn legacy_helper_sh() -> PathBuf {
    smart_dir_no_create()
        .join("bin")
        .join("claude-smart-helper.sh")
}

/// `~/.config/claude-as/profiles.json` — cross-platform profile→dir map.
/// Personal-only; absent on toss machines → binary falls back to current
/// `CLAUDE_CONFIG_DIR` and disables CAS/pick features.
pub fn profiles_json() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("claude-as")
        .join("profiles.json")
}

/// `~/.config/claude-smart/config.json` — csm's OWN global config (drop-in
/// launch command + future settings). Distinct from `~/.config/claude-as/`,
/// which is the profile-switch contract shared with the `cas` shell shims;
/// this is csm's own runtime config, not part of that contract.
pub fn config_json() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("claude-smart")
        .join("config.json")
}

/// Hub-local usage limits cache (read directly when this machine IS the hub —
/// the `CLAUDE_HUB_HOSTNAME` fast path).
/// `$HOME/claude-code-usage/cache/usage-limits.json`
pub fn hub_local_cache() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("claude-code-usage")
        .join("cache")
        .join("usage-limits.json")
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

/// `$HOME/.claude.shared/projects` — transcript projects base directory.
pub fn session_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.shared")
        .join("projects")
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
