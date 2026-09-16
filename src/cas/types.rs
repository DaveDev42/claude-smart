//! CAS operation and shell grammar types.

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

    // ─── registry management (non-eval; routed to `manage_emit`) ───────────────
    /// `cas list` — print the configured profiles (name → dir, default-marked).
    List,

    /// `cas add <name> [<dir>]` — register a profile, creating its dir. `dir`
    /// defaults to `~/.claude.<name>`. Errors if the name already exists.
    Add { name: String, dir: Option<String> },

    /// `cas set <name> <dir>` — register/overwrite a profile + create the dir.
    Set { name: String, dir: String },

    /// `cas remove <name>` / `cas rm <name>` — unregister a profile (the dir is
    /// retained on disk). Refused when `name` is the current global default.
    Remove { name: String },

    /// `cas use <name>` — set the global default (state file + platform floor),
    /// without emitting a per-shell export. The scriptable analogue of `-g`.
    SetDefault { name: String },

    /// `csm profiles edit` — interactive registry editor (TTY-gated menu loop).
    /// Non-eval; routed to `manage_emit`.
    Edit,
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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Shell export line tests ───────────────────────────────────────────────

    #[test]
    fn shell_zsh_export_line() {
        let line = Shell::Zsh.export_line("/Users/example/.claude.home");
        assert_eq!(
            line,
            "export CLAUDE_CONFIG_DIR='/Users/example/.claude.home'"
        );
    }

    #[test]
    fn shell_pwsh_export_line() {
        let line = Shell::Pwsh.export_line(r"C:\Users\example\.claude.home");
        assert_eq!(
            line,
            r"$env:CLAUDE_CONFIG_DIR = 'C:\Users\example\.claude.home'"
        );
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
        let _ = Op::Switch {
            profile: "home".to_owned(),
        };
        let _ = Op::Minus;
        let _ = Op::Global {
            profile: "work".to_owned(),
        };
        let _ = Op::Resync;
        let _ = Op::Status {
            print_current: false,
        };
        let _ = Op::Status {
            print_current: true,
        };
    }
}
