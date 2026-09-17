//! Shared subcommand helpers used across `main.rs`'s `cmd_*` entry points:
//! session-id generation, profile-dir resolution, the interactive-terminal
//! check, and the capped stdin reader used by `csm usage capture`.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::account;
use crate::paths;

/// Generate a fresh lowercase UUID v4 for use as `--session-id`.
pub(crate) fn newuuid() -> String {
    Uuid::new_v4().to_string()
}

// ─── profile resolution helpers ───────────────────────────────────────────────

/// Resolve a profile name → absolute `CLAUDE_CONFIG_DIR` path string.
///
/// Tries the ProfileMap first; synthesises a conventional path as fallback.
pub(crate) fn resolve_profile_dir(
    profile: &str,
    profiles: &account::ProfileMap,
) -> anyhow::Result<String> {
    if let Some(dir) = profiles.get(profile) {
        return Ok(dir.to_owned());
    }
    // Fallback: synthesise conventional `~/.claude.<profile>` path.
    Ok(paths::synthesize_profile_dir(profile)
        .to_string_lossy()
        .into_owned())
}

/// Return the current profile dir from `$CLAUDE_CONFIG_DIR`, or the default
/// resolved from the registry (`~/.config/claude-as/{profiles.json,default}`).
pub(crate) fn current_profile_dir(profiles: &account::ProfileMap) -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    profiles.default_dir()
}

/// Derive the current profile name from `$CLAUDE_CONFIG_DIR` + ProfileMap.
pub(crate) fn derive_current_profile_name(profiles: &account::ProfileMap) -> String {
    let dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    if dir.is_empty() {
        return profiles.default_name();
    }
    // Try to reverse-lookup the dir in the profiles map.
    if let Some((name, _)) = profiles.iter().find(|(_, d)| *d == dir.as_str()) {
        return name.to_owned();
    }
    // Derive from the directory basename (e.g. `.claude.home` → `personal`).
    std::path::Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_prefix(".claude.").unwrap_or(n).to_owned())
        .unwrap_or_else(|| profiles.default_name())
}

/// Reverse-lookup `dir` in `profiles` to a profile name, falling back to the
/// directory's basename with a `.claude.` prefix stripped (mirrors
/// `derive_current_profile_name`'s fallback, but keyed off an explicit `dir`
/// — the resolved launch target — rather than `$CLAUDE_CONFIG_DIR`).
pub(crate) fn profile_name_for_dir(dir: &Path, profiles: &account::ProfileMap) -> String {
    let dir_str = dir.to_string_lossy();
    if let Some((name, _)) = profiles.iter().find(|(_, d)| *d == dir_str.as_ref()) {
        return name.to_owned();
    }
    dir.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_prefix(".claude.").unwrap_or(n).to_owned())
        .unwrap_or_else(|| profiles.default_name())
}

/// `true` when both stdin and stdout are terminals — mirrors zsh `[[ -t 0 && -t 1 ]]`.
pub(crate) fn is_interactive() -> bool {
    #[cfg(unix)]
    {
        use nix::unistd::isatty;
        let stdin_ok = isatty(std::io::stdin()).unwrap_or(false);
        let stdout_ok = isatty(std::io::stdout()).unwrap_or(false);
        stdin_ok && stdout_ok
    }
    #[cfg(not(unix))]
    {
        // Windows: the `GetConsoleMode` check is unimplemented; the env
        // heuristic stands in.
        std::env::var("WT_SESSION").is_ok() || std::env::var("TERM").is_ok()
    }
}

/// Hard cap on how much of `csm usage capture`'s stdin (a statusLine JSON
/// payload) we will ever read. StatusLine payloads are small (a few KB at
/// most); this is purely a defensive ceiling against a misconfigured or
/// hostile pipe feeding an unbounded stream — see [`read_stdin_capped`].
pub(crate) const CAPTURE_STDIN_CAP_BYTES: u64 = 256 * 1024;

/// Read stdin up to `max_bytes`, lossily decoding as UTF-8. Never blocks past
/// EOF-or-cap; a payload larger than the cap is silently truncated (the
/// caller — a JSON parse — will simply fail on truncated input, which is
/// treated as a no-op by every caller here).
pub(crate) fn read_stdin_capped(max_bytes: u64) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(max_bytes).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── newuuid ───────────────────────────────────────────────────────────────

    #[test]
    fn newuuid_produces_lowercase_uuid() {
        let id = newuuid();
        assert_eq!(id.len(), 36, "UUID must be 36 chars");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        assert_eq!(id, id.to_lowercase(), "UUID must be lowercase");
    }

    #[test]
    fn newuuid_unique_each_call() {
        let a = newuuid();
        let b = newuuid();
        assert_ne!(a, b, "consecutive UUIDs must differ");
    }

    // ── is_interactive (smoke test — cannot assert value in non-tty env) ───────

    #[test]
    fn is_interactive_does_not_panic() {
        let _ = is_interactive();
    }

    // ── profile_name_for_dir ────────────────────────────────────────────────────

    #[test]
    fn profile_name_for_dir_reverse_looks_up_registry() {
        let profiles = account::ProfileMap(std::collections::HashMap::from([(
            "work".to_string(),
            "/Users/example/.claude.work".to_string(),
        )]));
        assert_eq!(
            profile_name_for_dir(Path::new("/Users/example/.claude.work"), &profiles),
            "work"
        );
    }

    #[test]
    fn profile_name_for_dir_falls_back_to_basename_strip() {
        let profiles = account::ProfileMap(std::collections::HashMap::new());
        assert_eq!(
            profile_name_for_dir(Path::new("/Users/example/.claude.home"), &profiles),
            "home"
        );
    }
}
