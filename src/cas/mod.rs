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
//! `eval_emit()` (this module) handles:
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
//! `default_profile()` — **REAL implementation** — reads
//! `~/.config/claude-as/default` and validates against the `personal|work`
//! allowlist, returning `"personal"` as the fallback for absent/unknown values.
//!
//! # Spec reference
//! `docs/superpowers/specs/2026-06-17-csm-rust-port-design.md` §2 "CAS integration"
//! `docs/superpowers/specs/2026-06-18-csm-rust-crate-scaffold.md` §3 `cas/mod.rs`

use std::io;
use std::path::PathBuf;

use crate::account::profiles::ProfileMap;

pub mod platform;

// ─── Op ──────────────────────────────────────────────────────────────────────

/// The parsed CAS operation; mirrors the `cas` shell function's argument forms.
///
/// ```text
/// cas <profile>        → Switch to the named profile in the live shell
/// cas -                → Switch back to the previous per-shell profile (handled
///                         entirely by the shim; binary receives the resolved name)
/// cas -g <profile>     → Global: write state file + launchctl/HKCU side-effects
/// cas resync           → Re-read state file and re-export in the live shell
/// cas status           → Print current / default / available (no export)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `cas <profile>` — per-shell switch: emit export, no state-file write.
    Switch { profile: String },

    /// `cas -` — toggle to the previous per-shell profile.
    /// The binary receives the literal `"-"` and emits a shell error snippet
    /// because `_CLAUDE_AS_PREV` is a non-exported zsh variable the child
    /// cannot read. The shim must handle `-` specially if it needs to preserve
    /// the previous-profile semantic; the binary's job is to emit the error.
    Minus,

    /// `cas -g <profile>` — global switch: write state file + platform setenv
    /// + emit export for the live shell.
    Global { profile: String },

    /// `cas resync` — re-read state file, emit export of its current value.
    Resync,

    /// `cas status [--print-current]` — informational; prints to stdout but
    /// emits no eval-able export line. `print_current` selects the one-liner
    /// form used by `_claude_as_current_profile` in the shim.
    Status { print_current: bool },
}

// ─── Shell ───────────────────────────────────────────────────────────────────

/// Which shell is evaluating the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Pwsh,
}

impl Shell {
    /// Parse `"zsh"` or `"pwsh"` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "zsh" | "bash" | "sh" => Some(Shell::Zsh),
            "pwsh" | "powershell" => Some(Shell::Pwsh),
            _ => None,
        }
    }

    /// Emit the `export` / `$env:` one-liner for the given dir.
    ///
    /// The path is single-quoted (safe for paths with spaces on both shells;
    /// single quotes in zsh/bash do not expand variables or globs; pwsh treats
    /// single-quoted strings as literals).
    pub fn export_line(&self, dir: &str) -> String {
        match self {
            Shell::Zsh => format!("export CLAUDE_CONFIG_DIR='{dir}'"),
            Shell::Pwsh => format!("$env:CLAUDE_CONFIG_DIR = '{dir}'"),
        }
    }

    /// Emit a shell snippet that prints `msg` to stderr and returns non-zero.
    ///
    /// This is used by `eval_emit` for terminal error cases so that `eval "$(csm
    /// cas ...)"` correctly surfaces the error to the calling shell even though
    /// `eval` discards the binary's exit code.
    ///
    /// - **zsh/bash**: `>&2 printf '%s\n' '<msg>'; false`
    /// - **pwsh**: `Write-Error '<msg>'; exit 1`
    ///
    /// Single quotes in `msg` are escaped per each shell's rules:
    /// - zsh: `'` → `'\''` (end-quote, literal-quote, re-open-quote)
    /// - pwsh: `'` → `''` (double-up)
    pub fn error_snippet(&self, msg: &str) -> String {
        match self {
            Shell::Zsh => {
                let escaped = msg.replace('\'', "'\\''");
                format!(">&2 printf '%s\\n' '{escaped}'; false")
            }
            Shell::Pwsh => {
                let escaped = msg.replace('\'', "''");
                format!("Write-Error '{escaped}'; exit 1")
            }
        }
    }

}

// ─── default state-file path ─────────────────────────────────────────────────

/// `~/.config/claude-as/default` — the global profile state file.
pub fn default_state_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("claude-as")
        .join("default")
}

// ─── default_profile — REAL implementation ───────────────────────────────────

/// Read the global default profile from `~/.config/claude-as/default`.
///
/// Returns the canonical profile name validated against the
/// `personal | work` allowlist. Falls back to `"personal"` when:
/// - the file is absent,
/// - the file is empty or contains only whitespace, or
/// - the file contains a value not in the allowlist.
///
/// This is the Rust equivalent of the zsh `_claude_as_default_profile`
/// function in `shared/zsh/claude-as.zsh` lines 33–42.
///
/// # Allowlist
/// Only `"personal"` and `"work"` are valid. Any other value is treated
/// as absent. This matches the `personal|work` SSOT that also drives the
/// `~/.zshenv` guard and the `launchd` floor.
pub fn default_profile() -> String {
    let path = default_state_file();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            match trimmed {
                "personal" | "work" => trimmed.to_owned(),
                _ => "personal".to_owned(),
            }
        }
        Err(_) => "personal".to_owned(),
    }
}

/// Write `profile` to the global default state file.
///
/// Creates the parent directory if needed. Validates against the allowlist
/// before writing — returns `Err` for an unknown profile so callers get a
/// clear error rather than a silently corrupted state file.
pub fn write_default_profile(profile: &str) -> io::Result<()> {
    if !is_valid_profile(profile) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cas: unknown profile '{profile}' — valid: personal, work"),
        ));
    }
    let path = default_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write without a trailing newline (matches the zsh `print -- "$profile" >`
    // idiom which adds exactly one newline; readers trim whitespace anyway).
    std::fs::write(&path, format!("{profile}\n"))
}

/// Returns `true` iff `profile` is in the `personal | work` allowlist.
pub fn is_valid_profile(profile: &str) -> bool {
    matches!(profile, "personal" | "work")
}

// ─── eval_emit ───────────────────────────────────────────────────────────────

/// Perform the CAS operation side-effects and print the eval-able output.
///
/// The caller (`cmd_cas` in `main.rs`) passes the parsed `Shell` and `Op`.
/// Output contract:
///
/// ## Normal paths (export line)
/// - `Op::Switch` / `Op::Global` / `Op::Resync`: print exactly **one** line
///   (the `export`/`$env:` line) to stdout. The parent shell evals it.
///
/// ## Informational path
/// - `Op::Status` (without `--print-current`): print human-readable status
///   directly to stdout — **not** eval-able; the shim must not eval this.
/// - `Op::Status { print_current: true }`: print the export of the global
///   default so the shim can eval it to set `_claude_as_current_profile`.
///
/// ## Error paths (emit shell error snippet)
/// - `Op::Minus`: emit a shell error snippet because `_CLAUDE_AS_PREV` is a
///   non-exported zsh variable the binary cannot read.
/// - Unknown profile: emit a shell error snippet instead of returning `Err`,
///   so that `eval "$(csm cas ...)"` correctly surfaces the error to the
///   calling shell.
///
/// # Platform side-effects (Op::Global only)
/// `Op::Global` also calls `platform::launchctl_setenv` (macOS) or
/// `platform::hkcu_setenv` (Windows) so GUI and launchd / non-shell
/// processes pick up the new default immediately.
///
/// # Return value
/// Returns `Err` only for I/O errors (state-file write failure, launchctl
/// exec failure, etc.) — NOT for user-level errors like unknown profile.
/// User-level errors are surfaced via the emitted shell error snippet.
pub fn eval_emit(shell: Shell, op: &Op, profiles: &ProfileMap) -> anyhow::Result<()> {
    match op {
        Op::Switch { profile } => {
            match resolve_profile(profile, profiles) {
                Ok(dir) => {
                    println!("{}", shell.export_line(&dir));
                }
                Err(e) => {
                    // Emit an error snippet so eval surfaces the error even
                    // though it discards the binary's exit code.
                    println!("{}", shell.error_snippet(&e.to_string()));
                }
            }
        }

        Op::Minus => {
            // _CLAUDE_AS_PREV is a non-exported zsh variable — the binary
            // cannot read it. Emit a shell error snippet that mirrors the zsh
            // `claude-as: no previous profile to toggle to` message.
            //
            // Reproduces zsh claude-as.zsh lines 79-84:
            //   if [[ -z "${_CLAUDE_AS_PREV:-}" ]]; then
            //     print -u2 "claude-as: no previous profile to toggle to"
            //     return 1
            //   fi
            println!(
                "{}",
                shell.error_snippet("claude-as: no previous profile to toggle to")
            );
        }

        Op::Global { profile } => {
            match resolve_profile(profile, profiles) {
                Ok(dir) => {
                    // 1. Write the state file.
                    write_default_profile(profile)?;
                    // 2. Platform-specific side-effect (launchctl / HKCU).
                    //    Soft failure: launchctl error does not abort the export.
                    if let Err(e) = platform::apply_global(profile, &dir) {
                        eprintln!("cas: platform setenv warning: {e}");
                    }
                    // 3. Emit the per-shell export line.
                    println!("{}", shell.export_line(&dir));
                    // 4. Print the informational message to stderr (matches
                    //    zsh `print "global default → $profile ($dir)"` which
                    //    goes to stdout in the original but is printed before
                    //    eval — here we print to stderr so it doesn't confuse
                    //    the eval).
                    eprintln!("global default → {profile} ({dir})");
                    eprintln!("(new shells follow this via ~/.zshenv guard; running claude sessions keep their captured paths)");
                }
                Err(e) => {
                    println!("{}", shell.error_snippet(&e.to_string()));
                }
            }
        }

        Op::Resync => {
            // Re-read the state file and emit the export for its current value.
            // Reproduces zsh claude-as.zsh lines 68-76:
            //   def=$(_claude_as_default_profile)
            //   def_dir="${CLAUDE_PROFILES[$def]}"
            //   _CLAUDE_AS_PREV=$(_claude_as_current_profile)
            //   export CLAUDE_CONFIG_DIR="$def_dir"
            //   print "shell → $def ($def_dir)"
            let profile = default_profile();
            match resolve_profile(&profile, profiles) {
                Ok(dir) => {
                    println!("{}", shell.export_line(&dir));
                    eprintln!("shell → {profile} ({dir})");
                }
                Err(e) => {
                    println!("{}", shell.error_snippet(&e.to_string()));
                }
            }
        }

        Op::Status { print_current } => {
            if *print_current {
                // 1-line form for `_claude_as_current_profile` shim helper.
                // The shim uses:
                //   _claude_as_current_profile() {
                //     command csm cas --eval --shell zsh -- status --print-current
                //   }
                // and evals the result to get the current profile name (not the
                // full export). We emit the profile name as a simple echo so
                // `eval` sets nothing (the shim reads it as output, not as a
                // command to eval).
                //
                // Actually the shim design says eval it — so we emit the profile
                // name as a zsh `echo` statement. But the comment in the scaffold
                // says this is for reading the current profile. Per the spec §3:
                //   _claude_as_current_profile() { command csm cas --eval --shell
                //     zsh -- status --print-current; }
                // This is NOT wrapped in eval — it's called directly. So we just
                // print the profile name (the one word the shell captures via
                // command substitution).
                let current_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
                let profile_name = if current_dir.is_empty() {
                    "unknown".to_owned()
                } else {
                    profiles
                        .iter()
                        .find(|(_, dir)| *dir == current_dir.as_str())
                        .map(|(name, _)| name.to_owned())
                        .unwrap_or_else(|| "unknown".to_owned())
                };
                println!("{profile_name}");
            } else {
                // Full status display — NOT eval-able.
                // Reproduces zsh claude-as.zsh lines 87-110 (no-args branch).
                print_status(shell, profiles)?;
            }
        }
    }
    Ok(())
}

/// Resolve a profile name to its `CLAUDE_CONFIG_DIR` path.
///
/// Falls back to constructing `~/.claude.<profile>` when `profiles` is empty
/// (toss machines / first-boot). An unknown profile in a populated map is an
/// error.
fn resolve_profile(profile: &str, profiles: &ProfileMap) -> anyhow::Result<String> {
    if profiles.is_empty() {
        // Toss machine or pre-ansible boot: synthesize the conventional path.
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cas: cannot determine HOME directory"))?;
        return Ok(home.join(format!(".claude.{profile}")).to_string_lossy().into_owned());
    }
    profiles
        .get(profile)
        .map(str::to_owned)
        .ok_or_else(|| {
            let available: Vec<&str> = profiles.names_sorted();
            anyhow::anyhow!(
                "cas: unknown profile '{}' — available: {}",
                profile,
                available.join(", ")
            )
        })
}

/// Print informational status (no eval output). Mirrors the zsh `cas` with no
/// args output (lines 87-110 of claude-as.zsh).
fn print_status(_shell: Shell, profiles: &ProfileMap) -> anyhow::Result<()> {
    // The live shell's CLAUDE_CONFIG_DIR is read from the environment.
    // The binary does not have a "previous profile" concept (that lives in the
    // shell's `_CLAUDE_AS_PREV` var). We render what we can.
    let current_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    let default = default_profile();
    let default_dir = if profiles.is_empty() {
        dirs::home_dir()
            .map(|h| h.join(format!(".claude.{default}")).to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        profiles.get(&default).unwrap_or("").to_owned()
    };

    // Resolve current profile name from CLAUDE_CONFIG_DIR.
    // Reproduces zsh `_claude_as_current_profile` (lines 46-54).
    let current_name = if current_dir.is_empty() {
        "unset".to_owned()
    } else {
        profiles
            .iter()
            .find(|(_, dir)| *dir == current_dir.as_str())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    };

    // Reproduces zsh lines 93-110:
    //   print "current shell:  $current_profile ($shell_state)"
    //   print "global default: $default_profile (~/.config/claude-as/default)"
    //   print "available:"
    //   for k in ${(ko)CLAUDE_PROFILES}; do
    //     mark=" "; [[ "$k" == "$current_profile" ]] && mark="*"
    //     [[ "$k" == "$default_profile" ]] && mark="${mark}d" || mark="${mark} "
    //     printf '  %s %-12s %s\n' "$mark" "$k" "$dir"
    //   done
    //   print "(legend: * = current shell, d = global default)"
    let shell_state = if current_dir.is_empty() {
        "unset (no zshenv guard? new shells won't have a profile)".to_owned()
    } else {
        current_dir.clone()
    };

    println!("current shell:  {current_name} ({shell_state})");
    // Show the RESOLVED config dir of the default profile (not a hardcoded path),
    // so the user can see where the default actually points — falls back to the
    // pointer-file location when the dir can't be resolved.
    if default_dir.is_empty() {
        println!("global default: {default} (~/.config/claude-as/default)");
    } else {
        println!("global default: {default} ({default_dir})");
    }
    println!("available:");

    if profiles.is_empty() {
        println!("  (profiles.json absent — CAS/pick features disabled)");
    } else {
        for name in profiles.names_sorted() {
            let dir = profiles.get(name).unwrap_or("");
            let is_current = dir == current_dir.as_str();
            let is_default = name == default.as_str();
            let mark = match (is_current, is_default) {
                (true,  true)  => "*d",
                (true,  false) => "* ",
                (false, true)  => " d",
                (false, false) => "  ",
            };
            println!("  {mark} {name:<12} {dir}");
        }
        println!("(legend: * = current shell, d = global default)");
    }

    Ok(())
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
        m.insert("personal".to_owned(), "/tmp/.claude.personal".to_owned());
        m.insert("work".to_owned(), "/tmp/.claude.work".to_owned());
        ProfileMap(m)
    }

    fn empty_profiles() -> ProfileMap {
        ProfileMap::default()
    }

    // ── default_profile tests ─────────────────────────────────────────────────

    /// Helper: apply the validation logic directly (mirrors what default_profile()
    /// does after reading the file) without touching the real state file.
    fn validate_profile_content(content: &str) -> String {
        let trimmed = content.trim();
        match trimmed {
            "personal" | "work" => trimmed.to_owned(),
            _ => "personal".to_owned(),
        }
    }

    #[test]
    fn default_profile_personal_roundtrip() {
        assert_eq!(validate_profile_content("personal\n"), "personal");
        assert_eq!(validate_profile_content("personal"), "personal");
    }

    #[test]
    fn default_profile_work_roundtrip() {
        assert_eq!(validate_profile_content("work\n"), "work");
        assert_eq!(validate_profile_content("work"), "work");
    }

    #[test]
    fn default_profile_unknown_falls_back_to_personal() {
        assert_eq!(validate_profile_content("hacker"), "personal");
        assert_eq!(validate_profile_content("toss"), "personal");
        assert_eq!(validate_profile_content(""), "personal");
        assert_eq!(validate_profile_content("  "), "personal");
    }

    #[test]
    fn default_profile_whitespace_trimmed() {
        // Leading/trailing whitespace is trimmed before the match.
        assert_eq!(validate_profile_content("  work  "), "work");
        assert_eq!(validate_profile_content("\tpersonal\n"), "personal");
    }

    #[test]
    fn write_default_profile_roundtrip_via_file() {
        // Write to a temp file and read it back directly (bypassing the fixed
        // path so we don't mutate real system state in tests).
        let mut f = NamedTempFile::new().unwrap();
        let profile = "work";
        write!(f, "{profile}\n").unwrap();
        let s = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(validate_profile_content(&s), "work");
    }

    #[test]
    fn is_valid_profile_allows_exactly_two() {
        assert!(is_valid_profile("personal"));
        assert!(is_valid_profile("work"));
        assert!(!is_valid_profile("toss"));
        assert!(!is_valid_profile(""));
        assert!(!is_valid_profile("PERSONAL")); // case-sensitive
        assert!(!is_valid_profile("personal "));
    }

    // ── Shell export line tests ───────────────────────────────────────────────

    #[test]
    fn shell_zsh_export_line() {
        let line = Shell::Zsh.export_line("/Users/example/.claude.personal");
        assert_eq!(line, "export CLAUDE_CONFIG_DIR='/Users/example/.claude.personal'");
    }

    #[test]
    fn shell_pwsh_export_line() {
        let line = Shell::Pwsh.export_line(r"C:\Users\example\.claude.personal");
        assert_eq!(line, r"$env:CLAUDE_CONFIG_DIR = 'C:\Users\example\.claude.personal'");
    }

    #[test]
    fn shell_parse_zsh_variants() {
        assert_eq!(Shell::parse("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("bash"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("sh"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("ZSH"), Some(Shell::Zsh));
    }

    #[test]
    fn shell_parse_pwsh_variants() {
        assert_eq!(Shell::parse("pwsh"), Some(Shell::Pwsh));
        assert_eq!(Shell::parse("powershell"), Some(Shell::Pwsh));
        assert_eq!(Shell::parse("PWSH"), Some(Shell::Pwsh));
    }

    #[test]
    fn shell_parse_unknown_is_none() {
        assert_eq!(Shell::parse("fish"), None);
        assert_eq!(Shell::parse(""), None);
    }

    // ── Shell error_snippet tests ─────────────────────────────────────────────

    #[test]
    fn shell_zsh_error_snippet_basic() {
        let s = Shell::Zsh.error_snippet("cas: unknown profile 'foo'");
        assert!(s.starts_with(">&2 printf"), "got: {s}");
        assert!(s.ends_with("; false"), "got: {s}");
        assert!(s.contains("unknown profile"), "got: {s}");
    }

    #[test]
    fn shell_pwsh_error_snippet_basic() {
        let s = Shell::Pwsh.error_snippet("cas: unknown profile 'foo'");
        assert!(s.starts_with("Write-Error"), "got: {s}");
        assert!(s.ends_with("exit 1"), "got: {s}");
        assert!(s.contains("unknown profile"), "got: {s}");
    }

    #[test]
    fn shell_zsh_error_snippet_quote_escaping() {
        // Single quotes in the message must be escaped for zsh single-quoting.
        let s = Shell::Zsh.error_snippet("it's a problem");
        // The escaped form should not break the shell string.
        assert!(s.contains("it'\\''s"), "expected zsh escape, got: {s}");
    }

    #[test]
    fn shell_pwsh_error_snippet_quote_escaping() {
        // Single quotes doubled in pwsh.
        let s = Shell::Pwsh.error_snippet("it's a problem");
        assert!(s.contains("it''s"), "expected pwsh double-quote, got: {s}");
    }

    // ── Op variants compile and are constructible ─────────────────────────────

    #[test]
    fn op_variants_constructible() {
        let _ = Op::Switch { profile: "personal".to_owned() };
        let _ = Op::Minus;
        let _ = Op::Global { profile: "work".to_owned() };
        let _ = Op::Resync;
        let _ = Op::Status { print_current: false };
        let _ = Op::Status { print_current: true };
    }

    // ── resolve_profile tests ─────────────────────────────────────────────────

    #[test]
    fn resolve_profile_personal_in_map() {
        let profiles = test_profiles();
        let result = resolve_profile("personal", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.personal");
    }

    #[test]
    fn resolve_profile_work_in_map() {
        let profiles = test_profiles();
        let result = resolve_profile("work", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.work");
    }

    #[test]
    fn resolve_profile_unknown_in_populated_map_errors() {
        let profiles = test_profiles();
        let result = resolve_profile("hacker", &profiles);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown profile"), "expected 'unknown profile' in: {msg}");
        assert!(msg.contains("personal"), "expected available profiles in: {msg}");
    }

    #[test]
    fn resolve_profile_empty_map_synthesizes_path() {
        let profiles = empty_profiles();
        let result = resolve_profile("personal", &profiles);
        assert!(result.is_ok());
        let dir = result.unwrap();
        // Should end with .claude.personal
        assert!(dir.ends_with(".claude.personal"), "synthesized dir: {dir}");
    }

    // ── eval_emit: Op::Minus emits error snippet ──────────────────────────────

    /// Capture what eval_emit prints to stdout by redirecting via a temp pipe.
    /// Since we can't redirect stdout in unit tests portably, we test the
    /// error snippet emission logic by calling `Shell::error_snippet` directly
    /// (which `eval_emit` delegates to for `Op::Minus`).
    #[test]
    fn op_minus_produces_error_snippet_for_zsh() {
        // Verify the snippet that will be emitted is well-formed zsh.
        let snippet = Shell::Zsh.error_snippet("claude-as: no previous profile to toggle to");
        assert!(snippet.contains("no previous profile"), "got: {snippet}");
        assert!(snippet.ends_with("; false"), "got: {snippet}");
    }

    #[test]
    fn op_minus_produces_error_snippet_for_pwsh() {
        let snippet = Shell::Pwsh.error_snippet("claude-as: no previous profile to toggle to");
        assert!(snippet.contains("no previous profile"), "got: {snippet}");
        assert!(snippet.ends_with("exit 1"), "got: {snippet}");
    }

    // ── eval_emit: allowlist rejection via error snippet ─────────────────────

    /// When an unknown profile is passed, eval_emit must NOT return Err (which
    /// would let the shim silently swallow the error) but must emit an error
    /// snippet so `eval` propagates the failure.
    ///
    /// We test by calling `resolve_profile` directly — the exact same logic
    /// eval_emit uses — to verify it returns Err, then verify
    /// `Shell::error_snippet` would wrap it into a valid snippet.
    #[test]
    fn unknown_profile_produces_error_snippet_not_panic() {
        let profiles = test_profiles();
        let err = resolve_profile("badprofile", &profiles).unwrap_err();
        let snippet = Shell::Zsh.error_snippet(&err.to_string());
        assert!(snippet.contains("unknown profile"), "got: {snippet}");
        assert!(snippet.ends_with("; false"), "got: {snippet}");
    }

    #[test]
    fn unknown_profile_error_snippet_for_pwsh() {
        let profiles = test_profiles();
        let err = resolve_profile("badprofile", &profiles).unwrap_err();
        let snippet = Shell::Pwsh.error_snippet(&err.to_string());
        assert!(snippet.contains("unknown profile"), "got: {snippet}");
        assert!(snippet.ends_with("exit 1"), "got: {snippet}");
    }

    // ── toggle logic tests ────────────────────────────────────────────────────

    /// The zsh toggle: if `_CLAUDE_AS_PREV` == "work" and user runs `cas -`,
    /// the shim resolves it to `cas work`. Here we verify that
    /// `resolve_profile("work", ...)` succeeds — the toggle logic itself
    /// is on the shim side, but the binary must handle the resolved profile name.
    #[test]
    fn toggle_resolves_previous_profile() {
        let profiles = test_profiles();
        // Simulate: user was on "personal", ran `cas work` (which sets
        // _CLAUDE_AS_PREV="personal"), then runs `cas -`.
        // The shim resolves _CLAUDE_AS_PREV to "personal" and calls csm with "personal".
        let result = resolve_profile("personal", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.personal");
    }

    /// Toggle to "work" (the other direction).
    #[test]
    fn toggle_resolves_other_profile() {
        let profiles = test_profiles();
        // Simulate: user was on "work", _CLAUDE_AS_PREV="work", `cas -`.
        // Actually: if current is personal and prev was work, toggle → work.
        let result = resolve_profile("work", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.work");
    }

    // ── resync logic ──────────────────────────────────────────────────────────

    /// Resync reads `default_profile()` and resolves it. Verify the logic
    /// produces the correct export line for a known profile (using the inner
    /// functions).
    #[test]
    fn resync_resolves_via_default_profile_logic() {
        let profiles = test_profiles();
        // Simulate default_profile() returning "personal":
        let profile = "personal";
        let dir = resolve_profile(profile, &profiles).unwrap();
        let line = Shell::Zsh.export_line(&dir);
        assert_eq!(line, "export CLAUDE_CONFIG_DIR='/tmp/.claude.personal'");
    }

    #[test]
    fn resync_pwsh_form() {
        let profiles = test_profiles();
        let dir = resolve_profile("work", &profiles).unwrap();
        let line = Shell::Pwsh.export_line(&dir);
        assert_eq!(line, "$env:CLAUDE_CONFIG_DIR = '/tmp/.claude.work'");
    }

    // ── global op: write_default_profile validation ───────────────────────────

    #[test]
    fn write_default_profile_rejects_unknown() {
        // write_default_profile validates against the allowlist before writing.
        let result = write_default_profile("toss");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown profile"), "got: {msg}");
    }

    #[test]
    fn write_default_profile_accepts_personal() {
        // We can test the allowlist validation path without writing to disk.
        // is_valid_profile is the gate; write_default_profile calls it first.
        assert!(is_valid_profile("personal"));
        assert!(is_valid_profile("work"));
        assert!(!is_valid_profile("toss"));
    }

    // ── parse_cas_args: arg parsing logic (exercised via parse_cas_args fn) ───

    #[test]
    fn parse_args_switch_personal() {
        let args = ["--eval", "--shell", "zsh", "--", "personal"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Switch { profile: "personal".to_owned() });
    }

    #[test]
    fn parse_args_switch_work() {
        let args = ["--eval", "--shell", "zsh", "--", "work"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Switch { profile: "work".to_owned() });
    }

    #[test]
    fn parse_args_switch_pwsh() {
        let args = ["--eval", "--shell", "pwsh", "--", "personal"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Pwsh);
        assert_eq!(op, Op::Switch { profile: "personal".to_owned() });
    }

    #[test]
    fn parse_args_minus() {
        let args = ["--eval", "--shell", "zsh", "--", "-"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Minus);
    }

    #[test]
    fn parse_args_global() {
        let args = ["--eval", "--shell", "zsh", "--", "-g", "personal"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Global { profile: "personal".to_owned() });
    }

    #[test]
    fn parse_args_global_long_form() {
        let args = ["--eval", "--shell", "zsh", "--", "--global", "work"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Global { profile: "work".to_owned() });
    }

    #[test]
    fn parse_args_resync() {
        let args = ["--eval", "--shell", "zsh", "--", "resync"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Resync);
    }

    #[test]
    fn parse_args_status() {
        let args = ["--eval", "--shell", "zsh", "--", "status"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Status { print_current: false });
    }

    #[test]
    fn parse_args_status_print_current() {
        let args = ["--eval", "--shell", "zsh", "--", "status", "--print-current"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Status { print_current: true });
    }

    #[test]
    fn parse_args_no_shell_defaults_to_zsh() {
        // When --shell is absent (bare call), default to zsh.
        let args = ["--eval", "--", "personal"];
        let (shell, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(shell, Shell::Zsh);
        assert_eq!(op, Op::Switch { profile: "personal".to_owned() });
    }

    #[test]
    fn parse_args_no_args_is_status() {
        // Bare `csm cas` (no --shell, no --) → status.
        let args: [&str; 0] = [];
        let (_, op) = parse_cas_args_for_test(&args).unwrap();
        assert_eq!(op, Op::Status { print_current: false });
    }

    #[test]
    fn parse_args_global_missing_profile_errors() {
        let args = ["--eval", "--shell", "zsh", "--", "-g"];
        let result = parse_cas_args_for_test(&args);
        assert!(result.is_err(), "expected error for -g without profile");
    }
}

// ─── test-only grammar fixture ────────────────────────────────────────────────
//
// The production cas argument parser is `parse_cas_flags` + `parse_cas_op` in
// `main.rs` and is the SSOT. The function below is a TEST-ONLY fixture that
// mirrors the grammar so the 12 unit tests below can exercise the (Shell, Op)
// outcome combinations without needing access to main.rs's private functions.
// It is gated `#[cfg(test)]` so it never appears in release builds and does
// not produce a dead-code warning.

#[cfg(test)]
fn parse_cas_args_for_test<S: AsRef<str>>(args: &[S]) -> anyhow::Result<(Shell, Op)> {
    let mut shell = Shell::Zsh; // default
    let mut i = 0;
    let n = args.len();

    // Consume csm-level flags (--eval, --shell) before `--`.
    while i < n {
        let a = args[i].as_ref();
        match a {
            "--eval" => {
                i += 1;
            }
            "--shell" => {
                i += 1;
                if i >= n {
                    anyhow::bail!("cas: --shell requires an argument (zsh|bash|pwsh)");
                }
                let s = args[i].as_ref();
                shell = Shell::parse(s).ok_or_else(|| {
                    anyhow::anyhow!("cas: unknown shell '{}' — expected zsh, bash, or pwsh", s)
                })?;
                i += 1;
            }
            "--" => {
                i += 1;
                break;
            }
            _ => {
                break;
            }
        }
    }

    // Parse the user command (after `--` or the last csm flag).
    if i >= n {
        return Ok((shell, Op::Status { print_current: false }));
    }

    let cmd = args[i].as_ref();
    match cmd {
        "-" => Ok((shell, Op::Minus)),

        "resync" => Ok((shell, Op::Resync)),

        "status" => {
            let print_current = (i + 1 < n) && args[i + 1].as_ref() == "--print-current";
            Ok((shell, Op::Status { print_current }))
        }

        "-g" | "--global" => {
            i += 1;
            if i >= n {
                anyhow::bail!(
                    "cas: {} requires a profile name (personal|work)",
                    cmd
                );
            }
            let profile = args[i].as_ref().to_owned();
            Ok((shell, Op::Global { profile }))
        }

        profile => {
            Ok((shell, Op::Switch { profile: profile.to_owned() }))
        }
    }
}
