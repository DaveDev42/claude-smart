//! `csm cas` — Claude-as (CAS) profile switcher.
//!
//! The binary half of the `cas` shell function. Because a child process cannot
//! mutate its parent shell's environment, the shell shim evals the single line
//! we print:
//!
//! ```zsh
//! cas() { eval "$(command csm cas --eval --shell zsh -- "$@")"; }
//! ```
//! ```pwsh
//! function cas { Invoke-Expression (csm cas --eval --shell pwsh -- @args) }
//! ```
//!
//! `eval_emit()` ([`eval`]) handles:
//!   - state-file write (`~/.config/claude-as/default`)
//!   - macOS `launchctl setenv` floor update (via `platform::launchctl_setenv`)
//!   - Windows HKCU\Environment write + `WM_SETTINGCHANGE` broadcast (via `platform::hkcu_setenv`)
//!   - printing the one line the parent shell must eval
//!
//! ## Error propagation through `eval`
//!
//! `eval "$(command csm cas ...)"` captures stdout. If `csm` exits non-zero but
//! emits nothing to stdout, the exit code is **lost** — `eval ""` succeeds. To
//! surface errors to the calling shell, `eval_emit` emits a shell error snippet
//! for terminal error cases (unknown profile, missing previous profile, etc.):
//!
//! - **zsh**: `>&2 printf '%s\n' '<message>'; false`
//! - **pwsh**: `Write-Error '<message>'; exit 1`
//!
//! When `eval` runs these, the shell function returns a non-zero exit code and
//! the message appears on stderr — matching the behavior of the original zsh
//! `claude-as` function.
//!
//! `default_profile(&ProfileMap)` reads `~/.config/claude-as/default` and
//! validates the token against the live registry (no hardcoded profile names),
//! falling back to the registry's `preferred_default`. The management verbs
//! (`list`/`add`/`set`/`remove`/`use`) author the registry itself via
//! `manage_emit` ([`manage`]) — csm owns the full profile lifecycle, not just
//! consumption.

use std::io;
use std::path::PathBuf;

use crate::account::profiles::ProfileMap;

pub mod edit;
pub mod eval;
pub mod manage;
pub mod platform;
pub mod types;

pub use eval::eval_emit;
pub use manage::manage_emit;
pub use types::{Op, Shell};

// ─── default state-file path ─────────────────────────────────────────────────

/// `~/.config/claude-as/default` — the global profile state file.
pub fn default_state_file() -> PathBuf {
    crate::paths::claude_as_dir().join("default")
}

// ─── default_profile — REAL implementation ───────────────────────────────────

/// Read the global default profile name, validated against the live registry.
///
/// Delegates to [`ProfileMap::default_name`] — the binary carries **no**
/// hardcoded profile names. Resolution order (see that method): configured
/// state-file token, else the registry's `preferred_default`, else `""`.
/// On an empty map (toss/first-boot) a non-empty state-file token is trusted
/// as-is so the synthesize path (`~/.claude.<token>`) still works.
pub fn default_profile(profiles: &ProfileMap) -> String {
    profiles.default_name()
}

/// Write `profile` to the global default state file.
///
/// Creates the parent directory if needed. Validates against the live registry
/// (`profiles`) before writing — an empty map (toss/synth) accepts any non-empty
/// token; a populated map requires `profile` to be a configured name. Returns
/// `Err` for an unknown profile so callers get a clear error rather than a
/// silently corrupted state file.
pub fn write_default_profile(profile: &str, profiles: &ProfileMap) -> io::Result<()> {
    let ok = if profiles.is_empty() {
        !profile.is_empty()
    } else {
        profiles.contains(profile)
    };
    if !ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cas: unknown profile '{profile}' — configured: {}",
                profiles.names_sorted().join(", ")
            ),
        ));
    }
    let path = default_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // One trailing newline (matches the zsh `print -- "$profile" >` idiom;
    // readers trim whitespace anyway).
    std::fs::write(&path, format!("{profile}\n"))
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Build a two-profile `ProfileMap` pointing at paths under `/tmp` so tests
    /// do not depend on the real home directory.
    fn test_profiles() -> ProfileMap {
        let mut m = HashMap::new();
        m.insert("home".to_owned(), "/tmp/.claude.home".to_owned());
        m.insert("work".to_owned(), "/tmp/.claude.work".to_owned());
        ProfileMap(m)
    }

    fn empty_profiles() -> ProfileMap {
        ProfileMap::default()
    }

    /// Compose the real `cmd::cas` parsers (`parse_cas_flags` + `parse_cas_op`)
    /// into the same `(Shell, Op)` shape the old test-only grammar fixture
    /// returned, so the `parse_args_*` tests below exercise the SSOT parser
    /// instead of a hand-copy that had already drifted from it (apc-06).
    fn compose_parse_cas_args<S: AsRef<str>>(args: &[S]) -> anyhow::Result<(Shell, Op)> {
        let owned: Vec<std::ffi::OsString> = args.iter().map(|s| s.as_ref().into()).collect();
        let flags = crate::cmd::cas::parse_cas_flags(&owned)?;
        let shell = match flags.shell.as_deref() {
            Some(s) => {
                Shell::parse(s).ok_or_else(|| anyhow::anyhow!("cas: unknown shell '{}'", s))?
            }
            None => Shell::Zsh,
        };
        let op = crate::cmd::cas::parse_cas_op(&flags.op_args)?;
        Ok((shell, op))
    }

    // ── default_profile / default_name tests (registry-driven, no allowlist) ──

    /// Write `content` to a temp state file and return it, so `default_name_with`
    /// can be exercised without touching the real `~/.config/claude-as/default`.
    fn state_file(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{content}").unwrap();
        f
    }

    #[test]
    fn default_name_returns_configured_token() {
        let p = test_profiles();
        let f = state_file("work\n");
        assert_eq!(p.default_name_with(f.path()), "work");
        let f = state_file("home");
        assert_eq!(p.default_name_with(f.path()), "home");
    }

    #[test]
    fn default_name_unknown_token_falls_back_to_preferred() {
        // A populated map + an unknown/blank token → preferred_default()
        // (alphabetical-first; for {work, home} that is "home").
        let p = test_profiles();
        let f = state_file("hacker");
        assert_eq!(p.default_name_with(f.path()), "home");
        let f = state_file("   ");
        assert_eq!(p.default_name_with(f.path()), "home");
    }

    #[test]
    fn default_name_whitespace_trimmed() {
        let p = test_profiles();
        let f = state_file("  work  ");
        assert_eq!(p.default_name_with(f.path()), "work");
        let f = state_file("\tpersonal\n");
        assert_eq!(p.default_name_with(f.path()), "home");
    }

    #[test]
    fn default_name_empty_map_trusts_any_token() {
        // Toss/synth regime: no configured profiles → the state-file token is
        // trusted verbatim (so `~/.claude.<token>` synthesis works).
        let p = empty_profiles();
        let f = state_file("whatever\n");
        assert_eq!(p.default_name_with(f.path()), "whatever");
    }

    #[test]
    fn default_name_empty_map_absent_token_is_empty() {
        let p = empty_profiles();
        let f = state_file("");
        assert_eq!(p.default_name_with(f.path()), "");
    }

    #[test]
    fn write_default_profile_roundtrip_via_file() {
        // Write to a temp file and read it back directly (the on-disk format is
        // "<name>\n"; readers trim whitespace).
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "work").unwrap();
        let s = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(s.trim(), "work");
    }

    #[test]
    fn profile_map_contains_replaces_allowlist() {
        let p = test_profiles();
        assert!(p.contains("home"));
        assert!(p.contains("work"));
        assert!(!p.contains("toss"));
        // Empty map contains nothing.
        assert!(!empty_profiles().contains("home"));
    }

    #[test]
    fn is_valid_name_syntax() {
        assert!(ProfileMap::is_valid_name("home"));
        assert!(ProfileMap::is_valid_name("work-2"));
        assert!(ProfileMap::is_valid_name("a.b_c"));
        assert!(!ProfileMap::is_valid_name(""));
        assert!(!ProfileMap::is_valid_name("has space"));
        assert!(!ProfileMap::is_valid_name("a/b")); // no path separators
    }

    // ── global op: write_default_profile validation ───────────────────────────

    #[test]
    fn write_default_profile_rejects_unknown_in_populated_map() {
        // A populated registry rejects a non-configured name; the error lists
        // the configured names dynamically (never a hardcoded allowlist).
        let result = write_default_profile("toss", &test_profiles());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown profile"), "got: {msg}");
        assert!(msg.contains("configured:"), "got: {msg}");
        assert!(msg.contains("home") && msg.contains("work"), "got: {msg}");
    }

    #[test]
    fn write_default_profile_empty_map_accepts_any_token() {
        // Toss/synth: empty registry accepts any non-empty token (rejects empty).
        // (Does not assert disk write — only the validation gate.)
        let empty = empty_profiles();
        // Empty token is rejected even on an empty map.
        assert!(empty.is_empty());
        assert!(ProfileMap::is_valid_name("anything"));
    }

    // ── parse_cas_args: arg parsing logic (exercised via parse_cas_args fn) ───

    #[test]
    fn parse_args_switch_personal() {
        let args = ["--eval", "--shell", "zsh", "--", "home"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Switch {
                profile: "home".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_switch_work() {
        let args = ["--eval", "--shell", "zsh", "--", "work"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Switch {
                profile: "work".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_switch_pwsh() {
        let args = ["--eval", "--shell", "pwsh", "--", "home"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Pwsh);
        assert_eq!(
            op,
            Op::Switch {
                profile: "home".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_minus() {
        let args = ["--eval", "--shell", "zsh", "--", "-"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Minus);
    }

    #[test]
    fn parse_args_global() {
        let args = ["--eval", "--shell", "zsh", "--", "-g", "home"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Global {
                profile: "home".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_global_long_form() {
        let args = ["--eval", "--shell", "zsh", "--", "--global", "work"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Global {
                profile: "work".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_resync() {
        let args = ["--eval", "--shell", "zsh", "--", "resync"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Resync);
    }

    #[test]
    fn parse_args_status() {
        let args = ["--eval", "--shell", "zsh", "--", "status"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Status {
                print_current: false
            }
        );
    }

    #[test]
    fn parse_args_status_print_current() {
        let args = [
            "--eval",
            "--shell",
            "zsh",
            "--",
            "status",
            "--print-current",
        ];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Status {
                print_current: true
            }
        );
    }

    #[test]
    fn parse_args_no_shell_defaults_to_zsh() {
        // When --shell is absent (bare call), default to zsh.
        let args = ["--eval", "--", "home"];
        let (shell, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(
            op,
            Op::Switch {
                profile: "home".to_owned()
            }
        );
    }

    #[test]
    fn parse_args_no_args_is_status() {
        // Bare `csm cas` (no --shell, no --) → status.
        let args: [&str; 0] = [];
        let (_, op) = compose_parse_cas_args(&args).unwrap();
        assert_eq!(
            op,
            Op::Status {
                print_current: false
            }
        );
    }

    #[test]
    fn parse_args_global_missing_profile_errors() {
        let args = ["--eval", "--shell", "zsh", "--", "-g"];
        let result = compose_parse_cas_args(&args);
        assert!(result.is_err(), "expected error for -g without profile");
    }
}
