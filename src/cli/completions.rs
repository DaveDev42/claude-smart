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
        /// Open the session picker.
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Start a fresh session (skip the session picker). Shadows claude's
        /// `-n, --name`.
        #[arg(short = 'n', long)]
        new: bool,
        /// Continue the newest free session.
        #[arg(short = 'c', long)]
        continue_: bool,
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
        /// Extra arguments forwarded verbatim to claude (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthru: Vec<String>,
    },

    /// Stop/SubagentStop/SessionEnd hook (reads event JSON from stdin).
    #[command(name = "hook")]
    Hook {
        /// Config directory that owns this hook instance (defaults to CLAUDE_CONFIG_DIR, then D).
        #[arg(long, value_name = "DIR")]
        owner: Option<String>,
    },

    /// Deprecated. `--print-default-dir` prints csm's runtime dir; `--eval`
    /// prints nothing (the profile switcher is gone).
    #[command(name = "cas", hide = true)]
    Cas {
        /// Deprecated no-op (prints nothing on stdout).
        #[arg(long)]
        eval: bool,
        /// Ignored.
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,
        /// Print csm's runtime dir `D` and exit.
        #[arg(long)]
        print_default_dir: bool,
        /// Ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        op_args: Vec<String>,
    },

    /// Orca's Claude accounts: list, switch, add, import, remove, check.
    #[command(name = "accounts")]
    Accounts {
        #[command(subcommand)]
        verb: Option<AccountsVerb>,
    },

    /// Orca interop: status, and the `claude` alias for Orca's command override.
    #[command(name = "orca")]
    Orca {
        #[command(subcommand)]
        verb: Option<OrcaVerb>,
    },

    /// Move a profile-based install onto Orca's account model.
    #[command(name = "migrate")]
    Migrate {
        #[command(subcommand)]
        verb: Option<MigrateVerb>,
    },

    /// csm's own global config (~/.config/claude-smart/config.json).
    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        verb: Option<ConfigVerb>,
    },

    /// Multi-account usage table (Orca accounts ∪ local view), offline-aware.
    #[command(name = "usage")]
    Usage {
        /// Emit the joined registry∪local view as JSON.
        #[arg(long)]
        json: bool,
        /// Read only the local cache (no network).
        #[arg(long)]
        no_fetch: bool,
        /// Bypass the cache and every profile's own TTL; re-probe live.
        #[arg(long)]
        refresh: bool,
        #[command(subcommand)]
        verb: Option<UsageVerb>,
    },

    /// Scan a directory for Claude Code sessions and print TSV rows.
    #[command(name = "scan")]
    Scan {
        /// Working directory to scan (defaults to current directory).
        #[arg(value_name = "CWD")]
        cwd: Option<String>,
    },

    /// Discover and kill orphan processes left behind by a csm-managed claude session.
    #[command(name = "reap")]
    Reap {
        /// List candidates and exit without a picker or any kill.
        #[arg(long)]
        dry_run: bool,
        /// Send SIGTERM instead of the default SIGKILL (POSIX; ignored on Windows).
        #[arg(long)]
        term: bool,
        /// Inspect every csm-managed session (the default scope).
        #[arg(long)]
        all: bool,
        /// Inspect a single session by id.
        #[arg(long, value_name = "SID")]
        session: Option<String>,
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

    /// Render the Claude Code statusLine segment.
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

    /// Run claude in csm's runtime dir, arguments forwarded verbatim.
    #[command(name = "claude")]
    Claude {
        /// Arguments handed to claude untouched (no csm parsing at all).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `csm usage <verb>` — the statusLine-stdin capture subverb, distinct from
/// `csm usage`'s own flags (`--json`/`--no-fetch`/`--refresh`).
#[derive(clap::Subcommand)]
pub enum UsageVerb {
    /// Read statusLine JSON from stdin, merge into the local store.
    #[command(name = "capture")]
    Capture,
}

/// `csm accounts <verb>`.
#[derive(clap::Subcommand)]
pub enum AccountsVerb {
    /// List Orca's host accounts (active and D's account marked).
    #[command(name = "list")]
    List {
        /// Identity only: skip the usage fetch.
        #[arg(long = "no-usage")]
        no_usage: bool,
    },
    /// Switch the active account (Orca RPC when Orca runs, else offline).
    #[command(name = "use")]
    Use {
        /// Account id, unique id prefix, or email.
        account: String,
    },
    /// Log in a new account (Orca's own flow when Orca runs).
    #[command(name = "add")]
    Add,
    /// Import the login held by one or more existing config dirs.
    #[command(name = "import")]
    Import {
        /// Config dirs to import from.
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Remove a non-active account.
    #[command(name = "rm")]
    Rm {
        /// Account id, unique id prefix, or email.
        account: String,
    },
    /// Check the account store, stashes, quarantine and D (never prints secrets).
    #[command(name = "doctor")]
    Doctor {
        /// Repair what can be repaired safely.
        #[arg(long)]
        fix: bool,
        /// Skip the profile check that needs the network.
        #[arg(long)]
        offline: bool,
    },
}

/// `csm orca <verb>`.
#[derive(clap::Subcommand)]
pub enum OrcaVerb {
    /// Show Orca's state as csm sees it (never prints secrets).
    #[command(name = "status")]
    Status,
    /// Create the `claude` alias for Orca's `agentCmdOverrides.claude`.
    #[command(name = "setup")]
    Setup,
}

/// `csm migrate <verb>`.
#[derive(clap::Subcommand)]
pub enum MigrateVerb {
    /// Read-only: what `import` and `retire` would do.
    #[command(name = "plan")]
    Plan,
    /// Import profile logins into Orca and carry config over to ~/.claude.
    #[command(name = "import")]
    Import {
        /// Print what would be done and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Retire migrated profile dirs and the old profile registry.
    #[command(name = "retire")]
    Retire {
        /// Print what would be done and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// `csm config <verb>` — global config verbs.
#[derive(clap::Subcommand)]
pub enum ConfigVerb {
    /// Print the config JSON (bare `csm config` ≡ show).
    #[command(name = "show")]
    Show,
    /// Print the resolved value of a config key.
    #[command(name = "get")]
    Get {
        /// Config key: launch-command or min-claude-version.
        key: String,
    },
    /// Set a config key. e.g. `set launch-command happy`,
    /// `set min-claude-version 2.1.283`.
    #[command(name = "set")]
    Set {
        /// Config key: launch-command or min-claude-version.
        key: String,
        /// Value tokens (the launch command argv).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        value: Vec<String>,
    },
    /// Clear a config key (revert to default). e.g. `unset launch-command`.
    #[command(name = "unset")]
    Unset {
        /// Config key: launch-command or min-claude-version.
        key: String,
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

    /// `csm usage --refresh-oauth` is rejected as an unknown flag, so no
    /// shell may offer it.
    #[test]
    fn completions_never_offer_refresh_oauth() {
        for shell in [Shell::Zsh, Shell::Bash, Shell::PowerShell] {
            let mut buf = Vec::new();
            generate(shell, &mut buf);
            let text = String::from_utf8_lossy(&buf);
            assert!(!text.contains("refresh-oauth"), "{shell:?}");
        }
    }

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
            "config",
            "usage",
            "accounts",
            "orca",
            "migrate",
            "scan",
            "reap",
            "sidecar",
            "statusline",
            "completions",
            "newuuid",
            "claude",
        ] {
            assert!(
                out.contains(sub),
                "zsh completions missing subcommand {sub:?}"
            );
        }
    }

    // ── `csm usage`'s own surface (--refresh, capture) is represented ─────────
    //
    // Regression coverage: the `Usage` variant's fields previously lagged
    // `main.rs::cmd_usage`'s real flags, so `csm usage --ref<TAB>` and `csm
    // usage cap<TAB>` silently offered nothing. `zsh_completions_include_all_subcommands`
    // above only checks top-level subcommand names, so it alone would not
    // have caught that drift.

    #[test]
    fn zsh_completions_include_usage_refresh_flag_and_capture_subverb() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("refresh"),
            "zsh completions missing `csm usage --refresh`"
        );
        assert!(
            out.contains("capture"),
            "zsh completions missing `csm usage capture`"
        );
    }

    // ── completions tree matches the reserved subcommand set ──────────────────

    #[test]
    fn completions_tree_matches_reserved_set() {
        use crate::cli::reserved::CSM_RESERVED_SUBCOMMANDS;

        let cmd = CsmCompletionsApp::command();
        // Hidden subcommands (the deprecated `cas`) still count: dispatch
        // still reserves the word.
        let mut names: Vec<&str> = cmd
            .get_subcommands()
            .map(|c| c.get_name())
            .filter(|n| *n != "help")
            .collect();
        names.sort_unstable();

        let mut expected: Vec<&str> = CSM_RESERVED_SUBCOMMANDS.to_vec();
        expected.sort_unstable();

        assert_eq!(
            names, expected,
            "the completions clap tree has drifted from CSM_RESERVED_SUBCOMMANDS"
        );
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
