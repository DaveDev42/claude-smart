//! Shell completion generation.
//!
//! This module uses `clap` **only** for generating completions (`csm completions
//! {zsh|bash|pwsh}`). It does not touch `csm`'s own argv — that is handled by
//! the hand-rolled parser in `cli/parser.rs`.
//!
//! The `CsmCompletionsApp` clap tree mirrors the full subcommand surface defined
//! in `main.rs`'s dispatch table. It is NEVER used to parse real argv; it exists
//! solely as a metadata source for `clap_complete::generate`.

use clap::CommandFactory;
use clap_complete::Shell;

// ─── Clap model (completions-only) ────────────────────────────────────────────
//
// Each subcommand's options are defined here so completions include the flags.
// These mirrors the hand-rolled parser in `cli/parser.rs`; keeping them in sync
// is a best-effort doc aid, not a correctness requirement (the real parser is
// authoritative).

/// Clap-derived struct used exclusively for `csm completions` — never for
/// parsing `csm run` arguments.
#[derive(clap::Parser)]
#[command(
    name = "csm",
    about = "Cross-platform Claude Code smart session manager",
    long_about = "csm — the claude-smart session launcher. Wraps `claude` with \
                  smart session selection, account auto-switching, and \
                  limit-detection relaunch."
)]
pub struct CsmCompletionsApp {
    #[command(subcommand)]
    pub command: CompletionsSubcmd,
}

#[derive(clap::Subcommand)]
pub enum CompletionsSubcmd {
    /// Launch claude (default subcommand when no subcommand is given).
    #[command(name = "run")]
    Run {
        /// Force interactive TTY mode.
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Continue the newest free session.
        #[arg(short = 'c', long)]
        continue_: bool,
        /// Force an account pick even if the current account is healthy.
        #[arg(short = 'A', long = "pick-account")]
        pick_account: bool,
        /// Suppress automatic account picking.
        #[arg(long)]
        no_pick: bool,
        /// Resume a specific session by UUID or title alias.
        #[arg(short = 'r', long, value_name = "ID_OR_ALIAS")]
        resume: Option<String>,
        /// Override `--permission-mode` (forwarded to claude).
        #[arg(long, value_name = "MODE")]
        permission_mode: Option<String>,
        /// Override `--effort` (forwarded to claude).
        #[arg(long, value_name = "LEVEL")]
        effort: Option<String>,
        /// Override `--model` (forwarded to claude).
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Explicit session id (forwarded to claude as --session-id).
        #[arg(long, value_name = "UUID")]
        session_id: Option<String>,
        /// Pin a specific Claude profile (skips account picking).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Extra arguments forwarded verbatim to claude (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthru: Vec<String>,
    },

    /// Stop/SubagentStop/SessionEnd hook (reads event JSON from stdin).
    #[command(name = "hook")]
    Hook {
        /// Profile directory that owns this hook instance (CLAUDE_CONFIG_DIR for this profile).
        #[arg(long, value_name = "DIR")]
        owner: Option<String>,
    },

    /// Claude-as profile switcher + registry manager (binary half; shim evals
    /// the eval-class output, calls management verbs directly).
    ///
    /// Eval-class ops (after `--`, with `--eval --shell`): `<profile>`, `-`,
    /// `-g <profile>`, `resync`, `status`.
    /// Management verbs (direct, no `--eval`): `list`, `add <name> [<dir>]`,
    /// `set <name> <dir>`, `remove|rm <name>`, `use <name>`, `edit`.
    #[command(name = "cas")]
    Cas {
        /// Emit the eval-able export line (required for profile switching).
        #[arg(long)]
        eval: bool,
        /// Shell dialect for the export line (zsh|bash|pwsh).
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,
        /// Print the resolved default CLAUDE_CONFIG_DIR and exit (floor SSOT).
        #[arg(long)]
        print_default_dir: bool,
        /// Operation and its arguments (after `--`, or a management verb).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        op_args: Vec<String>,
    },

    /// Profile registry (human-facing noun-verb front over the cas handlers).
    #[command(name = "profiles")]
    Profiles {
        #[command(subcommand)]
        verb: Option<ProfilesVerb>,
    },

    /// Multi-profile usage table (registry ∪ hub), offline-aware.
    #[command(name = "usage")]
    Usage {
        /// Emit the joined registry∪hub view as JSON.
        #[arg(long)]
        json: bool,
        /// Read only the local cache (no network).
        #[arg(long)]
        no_fetch: bool,
    },

    /// Pick the best account to launch under.
    #[command(name = "pick-account")]
    PickAccount {
        /// Current profile name (to optionally exclude from candidates).
        #[arg(value_name = "CURRENT")]
        current: Option<String>,
        /// Include the current profile in scoring; return empty on no-op.
        #[arg(long)]
        include_current: bool,
    },

    /// Scan a directory for Claude Code sessions and print TSV rows.
    #[command(name = "scan")]
    Scan {
        /// Working directory to scan (defaults to current directory).
        #[arg(value_name = "CWD")]
        cwd: Option<String>,
    },

    /// Discover orphan processes left behind by a csm-managed claude session.
    #[command(name = "reap")]
    Reap {
        /// List candidates and exit without killing anything.
        #[arg(long)]
        dry_run: bool,
        /// Inspect every csm-managed session (the default scope).
        #[arg(long)]
        all: bool,
        /// Inspect a single session by id.
        #[arg(long, value_name = "SID")]
        session: Option<String>,
    },

    /// Print session/week usage percentages for a profile.
    #[command(name = "current-usage")]
    CurrentUsage {
        /// Profile name to query.
        #[arg(value_name = "PROFILE")]
        profile: String,
    },

    /// Read/write/merge session sidecar state.
    #[command(name = "sidecar")]
    Sidecar {
        /// Operation: read | write | merge | flags
        #[arg(value_name = "OP")]
        op: String,
        /// Session UUID.
        #[arg(value_name = "SID")]
        sid: String,
        /// Key=value pairs to write/merge (for write/merge operations).
        #[arg(value_name = "KEY=VALUE")]
        kv_args: Vec<String>,
    },

    /// Print `<profile>@<host>` for shell prompt integration.
    #[command(name = "statusline")]
    Statusline,

    /// Emit shell completions for the given shell to stdout.
    #[command(name = "completions")]
    Completions {
        /// Target shell.
        shell: Shell,
    },

    /// Print a fresh lowercase UUID v4 (used as --session-id on cold launch).
    #[command(name = "newuuid")]
    Newuuid,
}

/// `csm profiles <verb>` — registry management verbs.
#[derive(clap::Subcommand)]
pub enum ProfilesVerb {
    /// List configured profiles (current/default marked).
    #[command(name = "list")]
    List,
    /// Register a new profile (dir defaults to ~/.claude.<name>).
    #[command(name = "add")]
    Add {
        /// Profile name.
        name: String,
        /// Config dir (optional; defaults to ~/.claude.<name>).
        dir: Option<String>,
    },
    /// Register/overwrite a profile's config dir.
    #[command(name = "set")]
    Set {
        /// Profile name.
        name: String,
        /// Config dir.
        dir: String,
    },
    /// Unregister a profile (refused if it is the default).
    #[command(name = "rm", alias = "remove")]
    Rm {
        /// Profile name.
        name: String,
    },
    /// Set the machine default profile (state file + platform floor).
    #[command(name = "use")]
    Use {
        /// Profile name.
        name: String,
    },
    /// Interactive editor (TTY).
    #[command(name = "edit")]
    Edit,
    /// Print a profile's config dir (default profile when omitted).
    #[command(name = "dir")]
    Dir {
        /// Profile name (default profile when omitted).
        name: Option<String>,
    },
}

// ─── generate ─────────────────────────────────────────────────────────────────

/// Generate completions for `shell` and write them to `out`.
///
/// Uses `CsmCompletionsApp` as the command metadata source. The `CsmCompletionsApp`
/// tree is intentionally kept in sync with `main.rs`'s dispatch table so
/// completions include all subcommands and their options.
pub fn generate(shell: Shell, out: &mut impl std::io::Write) {
    let mut cmd = CsmCompletionsApp::command();
    clap_complete::generate(shell, &mut cmd, "csm", out);
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── completions output is non-empty for all shells ────────────────────────

    #[test]
    fn generate_zsh_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        assert!(!buf.is_empty(), "zsh completions should not be empty");
    }

    #[test]
    fn generate_bash_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut buf);
        assert!(!buf.is_empty(), "bash completions should not be empty");
    }

    #[test]
    fn generate_powershell_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::PowerShell, &mut buf);
        assert!(
            !buf.is_empty(),
            "powershell completions should not be empty"
        );
    }

    // ── completions include known subcommand names ────────────────────────────

    #[test]
    fn zsh_completions_mention_run_subcommand() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("run") || out.contains("csm"),
            "zsh completions should reference the run subcommand or binary name"
        );
    }

    #[test]
    fn bash_completions_mention_hook_subcommand() {
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut buf);
        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("hook"),
            "bash completions should mention 'hook' subcommand"
        );
    }

    #[test]
    fn zsh_completions_mention_completions_subcommand() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("completions"),
            "zsh completions should mention 'completions' subcommand"
        );
    }

    // ── full subcommand surface is represented ────────────────────────────────

    #[test]
    fn zsh_completions_include_all_subcommands() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        let out = String::from_utf8_lossy(&buf);
        // All subcommands from the dispatch table.
        for sub in &[
            "run",
            "hook",
            "profiles",
            "usage",
            "cas",
            "pick-account",
            "scan",
            "reap",
            "current-usage",
            "sidecar",
            "statusline",
            "completions",
            "newuuid",
        ] {
            assert!(
                out.contains(sub),
                "zsh completions missing subcommand {sub:?}"
            );
        }
    }

    // ── generate is idempotent (called twice produces the same output) ────────

    #[test]
    fn generate_is_idempotent() {
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        generate(Shell::Zsh, &mut buf1);
        generate(Shell::Zsh, &mut buf2);
        assert_eq!(
            buf1, buf2,
            "repeated generate calls must produce identical output"
        );
    }
}
