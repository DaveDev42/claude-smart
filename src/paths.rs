//! Paths SSOT — every state-dir-relative filename in one place.
//!
//! Rule: **no hardcoded path strings outside this module**. Every caller that
//! needs a file under csm's state dir uses the constructors here.
//!
//! The state dir ([`smart_dir`]) is `orca::fsx::state_dir`: `$XDG_STATE_HOME/csm`
//! (absolute values only), else `~/.local/state/csm`, on every unix including
//! macOS; `%LOCALAPPDATA%\csm` on Windows. The retired profile setup kept it
//! at `~/.claude.shared/smart`; `csm migrate import` moves the session
//! sidecars, `titles.tsv` and the scan indexes from there and leaves the
//! caches and per-profile records behind unread.
//!
//! Sessions live under csm's runtime dir `D` ([`runtime_dir`]): transcripts in
//! `<D>/projects`, the live-session registry in `<D>/sessions`.
//!
//! [`home_dir`] resolves the current user's home directory cross-platform.
//!
//! Test guard: under `cfg(test)` [`home_dir`] is the thread's test home
//! (`testenv::with_test_home`), and with none set a fixed path under the temp
//! dir. It never falls back to the real home, so no test can resolve csm's
//! real state dir, `~/.claude` or `~/.config/claude-smart`.

use std::io;
use std::path::{Path, PathBuf};

/// The one home-dir resolver every path constructor in this module (and
/// `orca::HostEnv::current`) uses.
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

/// Test build: the thread's test home, else [`no_test_home`]. Never the real
/// home.
#[cfg(test)]
pub(crate) fn home_dir() -> Option<PathBuf> {
    Some(crate::testenv::test_home().unwrap_or_else(no_test_home))
}

/// Where a test that set no test home lands: a fixed dir under the temp dir,
/// so a forgotten fixture writes into scratch space, never the real home.
#[cfg(test)]
pub(crate) fn no_test_home() -> PathBuf {
    std::env::temp_dir().join("csm-test-no-home")
}

/// The environment the state and runtime dirs resolve from.
#[cfg(not(test))]
fn host_env() -> Option<crate::orca::HostEnv> {
    crate::orca::HostEnv::current().ok()
}

/// Test build: a blank environment rooted at the test home (no real env var
/// is ever read).
#[cfg(test)]
fn host_env() -> Option<crate::orca::HostEnv> {
    let home = home_dir()?;
    Some(crate::orca::HostEnv::for_test(
        &home,
        crate::orca::HostOs::current(),
    ))
}

/// Return the state directory, creating it (0700) if it does not yet exist.
///
/// The "lazy create" contract: callers that only *read* state (e.g. the TTY-gate
/// check that peeks at `.usage-cache.json`) should call `smart_dir_no_create()`
/// to avoid spurious dir creation in non-interactive contexts. Writers call this.
pub fn smart_dir() -> io::Result<PathBuf> {
    let dir = smart_dir_no_create();
    crate::orca::fsx::create_dir_all(&dir, 0o700)?;
    Ok(dir)
}

/// Return csm's state directory path without creating it.
pub fn smart_dir_no_create() -> PathBuf {
    match host_env() {
        Some(env) => crate::orca::fsx::state_dir(&env),
        None => PathBuf::from(".").join(".local").join("state").join("csm"),
    }
}

/// csm's runtime dir `D`: claude's `CLAUDE_CONFIG_DIR` as csm's environment
/// has it (trimmed), else `~/.claude` (Orca's getRuntimePaths rule).
pub fn runtime_dir() -> PathBuf {
    match host_env() {
        Some(env) => {
            crate::orca::runtime::runtime_paths(env.claude_config_dir.as_deref(), &env.home, |p| {
                p.exists()
            })
            .config_dir
        }
        None => PathBuf::from(".").join(".claude"),
    }
}

// ─── session-level paths ──────────────────────────────────────────────────────

/// `<smart_dir>/<sid>.json` — sidecar (mode/effort/model/cwd/profile/hop).
pub fn sidecar(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.json"))
}

/// `<state>/sentinel/<sid>.json` — the limit-switch handoff sentinel the
/// hook writes and the supervisor consumes.
pub fn sentinel(sid: &str) -> PathBuf {
    smart_dir_no_create()
        .join("sentinel")
        .join(format!("{sid}.json"))
}

/// `<state>/follow/` — one follow file per csm-supervised peer session.
pub fn follow_dir() -> PathBuf {
    smart_dir_no_create().join("follow")
}

/// `<state>/follow/<sid>.json` — written by a leader for a peer on the capped
/// account; the peer relaunches at its next turn boundary.
pub fn follow(sid: &str) -> PathBuf {
    follow_dir().join(format!("{sid}.json"))
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

/// `<smart_dir>/<sid>.model-fallback` — one-shot-per-window-per-account
/// marker: this session already fell back to the fallback model on a
/// `week_fable` cap, so a further trip within the same weekly window on the
/// SAME account is suppressed silently instead of relaunching on the same
/// model again or escalating to an account switch. Content is `"<epoch>
/// <profile>"`: the epoch is checked for staleness against the CURRENT
/// `week_fable` window on every read, so once that window rolls over the
/// marker no longer counts and a fresh fallback can fire again; the profile
/// is checked against the account the session is CURRENTLY on, so a marker
/// left behind by an earlier account switch never suppresses a fallback on
/// the new account either. See `hook::detect::fable_fallback_model`,
/// `hook::detect::model_fallback_marker_is_stale` and
/// `hook::detect::model_fallback_marker_is_current`.
pub fn model_fallback(sid: &str) -> PathBuf {
    smart_dir_no_create().join(format!("{sid}.model-fallback"))
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

/// `<state>/last-identity` — the last `oauthAccount.accountUuid` csm saw in
/// `D`. A change is a switch event (whoever made it) and re-stamps
/// [`last_switch`] and [`last_identity_switch`].
pub fn last_identity() -> PathBuf {
    smart_dir_no_create().join("last-identity")
}

/// `<state>/.last-identity-switch` — when `D`'s identity last changed (bare
/// epoch). Usage-capture attribution keys on it, not on [`last_switch`]: the
/// cooldown claim re-stamps that one when a switch is only about to be
/// asked for, and a claim that ends in no switch must not orphan the
/// captures of every session already running.
pub fn last_identity_switch() -> PathBuf {
    smart_dir_no_create().join(".last-identity-switch")
}

/// `<smart_dir>/titles.tsv` — session-name alias index (`title \t sid \t mtime`).
pub fn titles_tsv() -> PathBuf {
    smart_dir_no_create().join("titles.tsv")
}

/// `~/.config/claude-smart/config.json` — csm's OWN global config (drop-in
/// launch command + future settings).
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
/// `profile` (an Orca account id) MUST already be validated via
/// [`crate::account::accounts::is_valid_key`] before it reaches
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

/// `<D>/projects` — where claude keeps session transcripts, and where csm's
/// session scanner and alias index read.
pub fn session_base_dir() -> PathBuf {
    runtime_dir().join("projects")
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
    fn state_dir_is_under_the_test_home() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let d = smart_dir_no_create();
            assert!(d.starts_with(tmp.path()), "got {}", d.display());
            assert!(d.ends_with("csm"), "got {}", d.display());
            assert_eq!(runtime_dir(), tmp.path().join(".claude"));
            assert_eq!(
                session_base_dir(),
                tmp.path().join(".claude").join("projects")
            );
        });
    }

    /// Guard: with no test home a test never resolves the real home, the
    /// real state dir or `~/.claude`.
    #[test]
    fn no_test_home_never_resolves_the_real_home() {
        crate::testenv::set_test_home(None);
        let home = home_dir().unwrap();
        assert!(home.starts_with(std::env::temp_dir()));
        if let Some(real) = dirs::home_dir() {
            assert_ne!(home, real);
            assert!(
                !smart_dir_no_create().starts_with(&real) || real.starts_with(std::env::temp_dir())
            );
            assert!(!runtime_dir().starts_with(&real) || real.starts_with(std::env::temp_dir()));
            assert!(!config_json().starts_with(&real) || real.starts_with(std::env::temp_dir()));
        }
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
            usage_store_dir().starts_with(smart_dir_no_create()),
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
        assert!(sentinel(sid).to_string_lossy().contains(sid));
        assert!(follow(sid).to_string_lossy().contains(sid));
        assert!(pid_file(sid).to_string_lossy().contains(sid));
        assert!(stop_flag(sid).to_string_lossy().contains(sid));
        assert!(switched(sid).to_string_lossy().contains(sid));
        assert!(detected(sid).to_string_lossy().contains(sid));
        assert!(model_fallback(sid).to_string_lossy().contains(sid));
    }
}
