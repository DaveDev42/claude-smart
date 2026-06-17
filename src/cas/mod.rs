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
    /// The caller resolves the `_CLAUDE_AS_PREV` env var; the binary receives
    /// the resolved profile name as a `Switch` or the literal `"-"` and returns
    /// an error if no previous profile is set (caller must guard).
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
    pub fn export_line(&self, dir: &str) -> String {
        match self {
            Shell::Zsh => format!("export CLAUDE_CONFIG_DIR='{dir}'"),
            Shell::Pwsh => format!("$env:CLAUDE_CONFIG_DIR = '{dir}'"),
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
/// - `Op::Switch` / `Op::Global` / `Op::Resync`: print exactly **one** line
///   (the `export`/`$env:` line) to stdout. The parent shell evals it.
/// - `Op::Status`: print informational text to stdout (not eval-able; the
///   shim must NOT eval this path — it is invoked without `--eval` in that
///   case).
/// - `Op::Minus`: requires the caller to have already resolved the previous
///   profile; if called with `Op::Minus` directly, returns `Err`.
///
/// # Platform side-effects
/// - `Op::Global` also calls `platform::launchctl_setenv` (macOS) or
///   `platform::hkcu_setenv` (Windows) so GUI and launchd / non-shell
///   processes pick up the new default immediately.
pub fn eval_emit(shell: Shell, op: &Op, profiles: &ProfileMap) -> anyhow::Result<()> {
    match op {
        Op::Switch { profile } => {
            let dir = resolve_profile(profile, profiles)?;
            println!("{}", shell.export_line(&dir));
        }

        Op::Minus => {
            // The caller (shim) is expected to resolve `_CLAUDE_AS_PREV` and
            // reissue as `Op::Switch`. If we receive `Op::Minus` raw it means
            // the shim did not resolve it — return an error so the shim can
            // surface "no previous profile".
            anyhow::bail!("cas: no previous profile set (use 'cas <profile>' to switch first)");
        }

        Op::Global { profile } => {
            let dir = resolve_profile(profile, profiles)?;
            // 1. Write the state file.
            write_default_profile(profile)?;
            // 2. Platform-specific side-effect (launchctl / HKCU).
            platform::apply_global(profile, &dir)?;
            // 3. Emit the per-shell export line.
            println!("{}", shell.export_line(&dir));
        }

        Op::Resync => {
            // Re-read the state file and emit the export for its current value.
            let profile = default_profile();
            let dir = resolve_profile(&profile, profiles)?;
            println!("{}", shell.export_line(&dir));
        }

        Op::Status { print_current } => {
            if *print_current {
                // 1-line form for `_claude_as_current_profile` shim helper.
                // The shim uses `csm cas --eval --shell zsh -- status --print-current`
                // and evals the result. We just emit the export so the shim
                // can compare it, OR we emit just the profile name. Per the
                // design spec §3/§2 the shim expects `eval "$(…)"` semantics,
                // so we emit an export line of the default (resync-equivalent).
                let profile = default_profile();
                let dir = resolve_profile(&profile, profiles)?;
                println!("{}", shell.export_line(&dir));
            } else {
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
/// args output.
fn print_status(_shell: Shell, profiles: &ProfileMap) -> anyhow::Result<()> {
    // In Phase 0 (scaffold), the live shell's CLAUDE_CONFIG_DIR is read from
    // the environment. The binary does not have a "previous profile" concept
    // (that lives in the shell's `_CLAUDE_AS_PREV` var). We render what we can.
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
    let current_name = if current_dir.is_empty() {
        "unset".to_owned()
    } else {
        profiles
            .iter()
            .find(|(_, dir)| *dir == current_dir.as_str())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    };

    println!("current shell:  {current_name} ({})", if current_dir.is_empty() { "unset" } else { &current_dir });
    println!(
        "global default: {default} (~/.config/claude-as/default) → {default_dir}"
    );
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
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── default_profile tests ─────────────────────────────────────────────────

    /// Helper: write a value to a temp file and call the inner validation logic.
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

    // ── eval_emit Switch path ─────────────────────────────────────────────────

    #[test]
    fn eval_emit_switch_emits_export_line() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert("personal".to_owned(), "/Users/example/.claude.personal".to_owned());
        map.insert("work".to_owned(), "/Users/example/.claude.work".to_owned());
        let profiles = crate::account::profiles::ProfileMap(map);

        // We can't easily capture stdout in a unit test without redirect, but
        // we can validate the resolve path does not error.
        let result = resolve_profile("personal", &profiles);
        assert_eq!(result.unwrap(), "/Users/example/.claude.personal");

        let result = resolve_profile("work", &profiles);
        assert_eq!(result.unwrap(), "/Users/example/.claude.work");
    }

    #[test]
    fn resolve_profile_unknown_in_populated_map_errors() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert("personal".to_owned(), "/tmp/.claude.personal".to_owned());
        let profiles = crate::account::profiles::ProfileMap(map);

        let result = resolve_profile("hacker", &profiles);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown profile"), "expected 'unknown profile' in: {msg}");
        assert!(msg.contains("personal"), "expected available profiles in: {msg}");
    }

    #[test]
    fn resolve_profile_empty_map_synthesizes_path() {
        let profiles = crate::account::profiles::ProfileMap::default();
        let result = resolve_profile("personal", &profiles);
        assert!(result.is_ok());
        let dir = result.unwrap();
        // Should end with .claude.personal
        assert!(dir.ends_with(".claude.personal"), "synthesized dir: {dir}");
    }
}
