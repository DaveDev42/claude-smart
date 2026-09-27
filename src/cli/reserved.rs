//! Single source of truth for the two disjoint reserved-subcommand-word sets
//! and the argv[0]-aware dispatch rule that picks between them.
//!
//! `CSM_RESERVED_SUBCOMMANDS` is the word list `main()` matches at `args[1]`;
//! `CLAUDE_RESERVED_SUBCOMMANDS` is claude's own list. The two must stay
//! disjoint (CLAUDE.md Invariant 2 — csm must never collide with a claude
//! subcommand). Any word not in the csm set falls through to an implicit
//! `csm run` and reaches `claude` verbatim. Nothing is peeled in front of the
//! word: there is no csm-global flag any more.
//!
//! argv[0] picks one of three rule sets ([`Invocation`]):
//! - `csm` — the csm words above.
//! - `csm-hook` — always `hook`.
//! - `claude` (the alias `csm orca setup` creates for Orca's
//!   `agentCmdOverrides.claude`) — csm words are never dispatched. A claude
//!   subcommand word at `args[1]` (every [`CLAUDE_RESERVED_SUBCOMMANDS`]
//!   entry), `--version`/`-v`/`-V` or `--help`/`-h` goes to the real claude
//!   verbatim (the `claude` passthrough), so Orca's version probe and hook
//!   installer see claude itself. Everything else is the implicit `run`.

use std::ffi::OsString;

/// Words `csm` treats as its own subcommand at `args[1]` (or `csm-hook` at
/// `argv[0]`, handled separately in [`dispatch_subcommand`]).
pub(crate) const CSM_RESERVED_SUBCOMMANDS: &[&str] = &[
    "run",
    "hook",
    "config",
    "usage",
    // Deprecated compat only: `--print-default-dir` and a no-op `--eval`.
    "cas",
    "scan",
    "sidecar",
    "statusline",
    "completions",
    "newuuid",
    "reap",
    "accounts",
    "orca",
    "migrate",
    // The documented passthrough verb. `claude` is NOT one of claude's own
    // subcommands (`claude claude` is not a thing), so reserving the word
    // costs nothing and gives `csm claude <args…>` — claude in csm's runtime
    // dir, arguments forwarded verbatim.
    "claude",
];

/// `claude`'s own reserved subcommand words, as `claude --help` lists them.
/// The disjointness test asserts csm never claims one, and the `claude`
/// argv[0] alias forwards every one of them to the real claude.
///
/// Keep it a superset rather than a minimal one: an entry csm must never claim
/// is cheap, and a missing entry is how a collision ships.
pub(crate) const CLAUDE_RESERVED_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "configuration",
    "doctor",
    "fix",
    "gateway",
    "import",
    "import-conversations",
    "install",
    "kill",
    "lists",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "rc",
    "remote-control",
    "respawn",
    "rm",
    "sandbox",
    "sessions",
    "setup-token",
    "stop",
    "terminal",
    "ultrareview",
    "update",
    "upgrade",
    "worktree",
];

/// Flags the `claude` alias hands to the real claude when they come first:
/// Orca's `reportVersion` probe runs `<override> --version`.
const CLAUDE_ALIAS_EXEC_FLAGS: &[&str] = &["--version", "-v", "-V", "--help", "-h"];

/// Which name csm was invoked under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// `csm` (or any other name).
    Csm,
    /// `csm-hook`.
    Hook,
    /// `claude` / `claude.exe` — the alias for Orca's command override.
    ClaudeAlias,
}

/// Classify `argv[0]` by its lowercased file stem.
pub(crate) fn invocation(args: &[OsString]) -> Invocation {
    let stem = args
        .first()
        .and_then(|a| {
            std::path::Path::new(a)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();
    match stem.as_str() {
        "csm-hook" => Invocation::Hook,
        "claude" => Invocation::ClaudeAlias,
        _ => Invocation::Csm,
    }
}

/// Where [`dispatch_subcommand`] routed an argument list.
///
/// `rest_len` is how many trailing args belong to the subcommand, so the
/// caller slices `&args[args.len() - rest_len..]` without re-deriving how many
/// words dispatch consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dispatch {
    /// The subcommand word to run (a `CSM_RESERVED_SUBCOMMANDS` entry, or
    /// `"run"` for the implicit fallthrough).
    pub(crate) subcommand: &'static str,
    /// Count of trailing args belonging to `subcommand`.
    pub(crate) rest_len: usize,
}

/// argv[0]-aware dispatch: which subcommand word to route to and how many
/// trailing args belong to it.
///
/// - `csm-hook` → `hook` with everything after argv[0] as `rest`.
/// - `claude` alias → `claude` (the passthrough) with `rest` = `args[1..]`
///   when `args[1]` is a claude subcommand word or a version/help flag,
///   else the implicit `run` with `rest` = `args[1..]`. csm's own words are
///   never dispatched under this name.
/// - `csm`: `args[1]` matched against `CSM_RESERVED_SUBCOMMANDS` → that word
///   with `rest` = `args[2..]`; anything else → the implicit `run` with
///   `rest` = everything after argv[0].
pub(crate) fn dispatch_subcommand(args: &[OsString]) -> Dispatch {
    let after_argv0 = args.len().saturating_sub(1);
    let first = args.get(1).map(|a| a.to_string_lossy());
    match invocation(args) {
        Invocation::Hook => Dispatch {
            subcommand: "hook",
            rest_len: after_argv0,
        },
        Invocation::ClaudeAlias => {
            let exec_claude = first.as_deref().is_some_and(|w| {
                CLAUDE_RESERVED_SUBCOMMANDS.contains(&w) || CLAUDE_ALIAS_EXEC_FLAGS.contains(&w)
            });
            Dispatch {
                subcommand: if exec_claude { "claude" } else { "run" },
                rest_len: after_argv0,
            }
        }
        Invocation::Csm => match first.as_deref().and_then(reserved_word) {
            Some(word) => Dispatch {
                subcommand: word,
                rest_len: args.len() - 2,
            },
            None => Dispatch {
                subcommand: "run",
                rest_len: after_argv0,
            },
        },
    }
}

/// The `CSM_RESERVED_SUBCOMMANDS` entry equal to `candidate`, if any.
fn reserved_word(candidate: &str) -> Option<&'static str> {
    CSM_RESERVED_SUBCOMMANDS
        .iter()
        .copied()
        .find(|w| *w == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replaces the old 7-of-12 hand-picked list in
    /// `dispatch_claude_subcommands_fall_through_to_run`: the full
    /// intersection of the two reserved sets must be empty.
    #[test]
    fn csm_and_claude_reserved_sets_are_disjoint() {
        for w in CSM_RESERVED_SUBCOMMANDS {
            assert!(
                !CLAUDE_RESERVED_SUBCOMMANDS.contains(w),
                "csm reserved word {w:?} collides with a claude subcommand"
            );
        }
    }

    /// `main()` must not let the top-level `--version`/`--help` interception
    /// pre-empt the `csm-hook` argv[0] alias — `csm-hook --version` has to
    /// reach `cmd_hook`, not print csm's own version.
    #[test]
    fn hook_alias_is_recognised_for_csm_hook_argv0() {
        let a = vec![OsString::from("csm-hook"), OsString::from("--version")];
        assert_eq!(invocation(&a), Invocation::Hook);
    }

    #[test]
    fn plain_csm_is_not_the_hook_alias() {
        let a = vec![OsString::from("csm"), OsString::from("--version")];
        assert_eq!(invocation(&a), Invocation::Csm);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Dispatch routing — verify that the argument dispatcher picks the right
    // subcommand word, covering the full table in main(). Exercises
    // `dispatch_subcommand`, the single tested source of truth for the
    // reserved word list. Pure-logic tests: no subprocess / real I/O / network
    // calls.
    // ══════════════════════════════════════════════════════════════════════════

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
    }

    /// Dispatch `ss` and return `(subcommand, rest)` — `rest` sliced exactly
    /// the way `main()` slices it, as plain strings for comparison.
    fn routed(ss: &[&str]) -> (&'static str, Vec<String>) {
        let a = argv(ss);
        let d = dispatch_subcommand(&a);
        let rest: Vec<String> = a[a.len() - d.rest_len..]
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        (d.subcommand, rest)
    }

    fn strings(ss: &[&str]) -> Vec<String> {
        ss.iter().map(|s| (*s).to_owned()).collect()
    }

    // ── explicit subcommands ──────────────────────────────────────────────────

    #[test]
    fn dispatch_explicit_hook() {
        let a = argv(&["csm", "hook", "--owner", "/Users/example/.claude.home"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "hook");
        assert_eq!(d.rest_len, 2);
    }

    #[test]
    fn dispatch_explicit_cas() {
        let a = argv(&["csm", "cas", "--eval", "--shell", "zsh", "--", "home"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "cas");
        assert_eq!(d.rest_len, 5);
    }

    #[test]
    fn dispatch_explicit_accounts_orca_migrate() {
        assert_eq!(
            routed(&["csm", "accounts", "use", "alice@example.com"]),
            ("accounts", strings(&["use", "alice@example.com"]))
        );
        assert_eq!(
            routed(&["csm", "orca", "status"]),
            ("orca", strings(&["status"]))
        );
        assert_eq!(
            routed(&["csm", "migrate", "plan"]),
            ("migrate", strings(&["plan"]))
        );
    }

    /// The retired words are no longer csm's: they reach claude as prompts.
    #[test]
    fn dispatch_retired_words_fall_through_to_run() {
        for w in ["profiles", "pick-account", "current-usage"] {
            assert_eq!(routed(&["csm", w]).0, "run", "{w}");
        }
    }

    #[test]
    fn dispatch_explicit_usage() {
        let a = argv(&["csm", "usage", "--json"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "usage");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_config() {
        let a = argv(&["csm", "config", "show"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "config");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_reap() {
        let a = argv(&["csm", "reap", "--dry-run"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "reap");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_claude_passthrough() {
        let a = argv(&["csm", "claude", "--version"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "claude");
        assert_eq!(d.rest_len, 1);
    }

    /// A word that is NOT a reserved csm subcommand falls through to `run`
    /// (→ forwarded to claude). This is the collision-avoidance contract: any
    /// claude subcommand (mcp/doctor/update/…) is forwarded, never hijacked.
    #[test]
    fn dispatch_claude_subcommands_fall_through_to_run() {
        for w in CLAUDE_RESERVED_SUBCOMMANDS {
            let a = argv(&["csm", w, "--some-flag"]);
            assert_eq!(
                dispatch_subcommand(&a).subcommand,
                "run",
                "`csm {w}` must fall through to run (forward to claude)"
            );
        }
    }

    #[test]
    fn dispatch_explicit_scan() {
        let a = argv(&["csm", "scan", "/tmp/project"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "scan");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_sidecar() {
        let a = argv(&["csm", "sidecar", "read", "abc-sid"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "sidecar");
        assert_eq!(d.rest_len, 2);
    }

    #[test]
    fn dispatch_explicit_statusline() {
        let a = argv(&["csm", "statusline"]);
        assert_eq!(dispatch_subcommand(&a).subcommand, "statusline");
    }

    #[test]
    fn dispatch_explicit_completions() {
        let a = argv(&["csm", "completions", "zsh"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "completions");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_newuuid() {
        let a = argv(&["csm", "newuuid"]);
        assert_eq!(dispatch_subcommand(&a).subcommand, "newuuid");
    }

    // ── implicit `run` fallthrough ────────────────────────────────────────────

    #[test]
    fn dispatch_bare_csm_is_run() {
        let a = argv(&["csm"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "run");
        assert_eq!(d.rest_len, 0);
    }

    #[test]
    fn dispatch_csm_flag_only_is_run() {
        let a = argv(&["csm", "-c"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "run");
        assert_eq!(d.rest_len, 1);
    }

    #[test]
    fn dispatch_unknown_subcommand_falls_through_to_run() {
        let a = argv(&["csm", "unknowncmd"]);
        assert_eq!(dispatch_subcommand(&a).subcommand, "run");
    }

    #[test]
    fn dispatch_explicit_run_subcommand() {
        let a = argv(&["csm", "run", "-c", "-n"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "run");
        assert_eq!(d.rest_len, 2);
    }

    /// There is no csm-global flag: a leading `--profile` is an ordinary
    /// launch token and the whole list goes to `run`.
    #[test]
    fn dispatch_leading_profile_flag_is_run() {
        assert_eq!(
            routed(&["csm", "--profile", "home", "statusline"]),
            ("run", strings(&["--profile", "home", "statusline"]))
        );
    }

    // ── the `claude` argv[0] alias ────────────────────────────────────────────

    #[test]
    fn invocation_by_stem() {
        assert_eq!(invocation(&argv(&["csm"])), Invocation::Csm);
        assert_eq!(invocation(&argv(&["/usr/local/bin/csm"])), Invocation::Csm);
        assert_eq!(invocation(&argv(&["csm-hook"])), Invocation::Hook);
        assert_eq!(invocation(&argv(&["claude"])), Invocation::ClaudeAlias);
        assert_eq!(
            invocation(&argv(&["/Users/example/.local/state/csm/bin/claude"])),
            Invocation::ClaudeAlias
        );
        assert_eq!(invocation(&argv(&["Claude.EXE"])), Invocation::ClaudeAlias);
        assert_eq!(invocation(&argv(&[])), Invocation::Csm);
    }

    /// Every claude subcommand word reaches the real claude verbatim under
    /// the alias, the word included.
    #[test]
    fn alias_forwards_every_claude_word_to_claude() {
        for w in CLAUDE_RESERVED_SUBCOMMANDS {
            assert_eq!(
                routed(&["claude", w, "--x"]),
                ("claude", strings(&[w, "--x"])),
                "`claude {w}` under the alias must exec the real claude"
            );
        }
    }

    /// Claude Code 2.1.283's top-level commands and their aliases, read from
    /// its command table: `claude upgrade` under the alias must update claude,
    /// not start a csm session.
    #[test]
    fn reserved_list_covers_claude_2_1_283_top_level_words() {
        for w in [
            "agents",
            "auth",
            "auto-mode",
            "doctor",
            "gateway",
            "import",
            "import-conversations",
            "install",
            "mcp",
            "plugin",
            "plugins",
            "project",
            "rc",
            "remote-control",
            "sandbox",
            "setup-token",
            "ultrareview",
            "update",
            "upgrade",
        ] {
            assert!(CLAUDE_RESERVED_SUBCOMMANDS.contains(&w), "missing {w}");
        }
    }

    #[test]
    fn alias_forwards_version_and_help_flags() {
        for f in ["--version", "-v", "-V", "--help", "-h"] {
            assert_eq!(routed(&["claude", f]), ("claude", strings(&[f])), "{f}");
        }
    }

    /// csm's own words are never dispatched under the alias: `claude usage`
    /// is a launch whose prompt is "usage".
    #[test]
    fn alias_never_dispatches_csm_words() {
        for w in CSM_RESERVED_SUBCOMMANDS {
            assert_eq!(
                routed(&["claude", w]),
                ("run", strings(&[w])),
                "`claude {w}` under the alias must be an implicit run"
            );
        }
    }

    #[test]
    fn alias_launch_shapes_go_to_run() {
        assert_eq!(routed(&["claude"]), ("run", vec![]));
        assert_eq!(
            routed(&["claude", "-n", "--resume", "abc"]),
            ("run", strings(&["-n", "--resume", "abc"]))
        );
        assert_eq!(
            routed(&["claude", "-p", "--version"]),
            ("run", strings(&["-p", "--version"]))
        );
    }
}
