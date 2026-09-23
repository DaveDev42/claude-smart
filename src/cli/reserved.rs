//! Single source of truth for the two disjoint reserved-subcommand-word sets
//! and the argv[0]-aware dispatch rule that picks between them.
//!
//! `CSM_RESERVED_SUBCOMMANDS` is the exact word list `main()` used to match
//! inline at `args[1]`; `CLAUDE_RESERVED_SUBCOMMANDS` exists only so tests can
//! assert the two sets stay disjoint (CLAUDE.md invariant 2 — csm must never
//! collide with a claude subcommand). Any word not in the csm set falls
//! through to an implicit `csm run` and reaches `claude` verbatim.
//!
//! One csm-global flag may sit in front of the subcommand word:
//! `csm --profile <name> <subcommand>`. [`dispatch_subcommand`] peels it off
//! and hands the name back so `main()` can pin `CLAUDE_CONFIG_DIR` before the
//! subcommand runs. Nothing else is peeled — every other leading token means
//! the whole argument list belongs to the implicit `run`.

use std::ffi::OsString;

/// Words `csm` treats as its own subcommand at `args[1]` (or `csm-hook` at
/// `argv[0]`, handled separately in [`dispatch_subcommand`]).
pub(crate) const CSM_RESERVED_SUBCOMMANDS: &[&str] = &[
    "run",
    "hook",
    "profiles",
    "config",
    "usage",
    "cas",
    "pick-account",
    "scan",
    "current-usage",
    "sidecar",
    "statusline",
    "completions",
    "newuuid",
    "reap",
    // Orca desktop-app interop. `claude orca` is not a claude subcommand.
    "orca",
    // The documented passthrough verb. `claude` is NOT one of claude's own
    // subcommands (`claude claude` is not a thing), so reserving the word
    // costs nothing and gives `csm claude <args…>` — claude under csm's
    // profile, arguments forwarded verbatim.
    "claude",
];

/// `claude`'s own reserved subcommand words, as `claude --help` lists them.
/// Never matched against by `dispatch_subcommand` — this list exists so
/// `csm_and_claude_reserved_sets_are_disjoint` can assert the two sets never
/// overlap, so it is otherwise dead outside `#[cfg(test)]`.
///
/// Keep it a superset rather than a minimal one: an entry csm must never claim
/// is cheap, and a missing entry is how a collision ships.
#[allow(dead_code)]
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
    "install",
    "kill",
    "lists",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "respawn",
    "rm",
    "sessions",
    "setup-token",
    "stop",
    "terminal",
    "ultrareview",
    "update",
    "worktree",
];

/// True when `argv[0]` is the `csm-hook` alias (symlink/rename form), the one
/// case where dispatch does not look at `args[1]` at all. `main()` uses this
/// to guard its top-level `--version`/`-V`/`--help`/`-h` interception so that
/// `csm-hook --version` still routes to `cmd_hook` instead of printing csm's
/// own version/help.
pub(crate) fn invoked_as_hook_alias(args: &[OsString]) -> bool {
    let argv0 = args
        .first()
        .and_then(|a| {
            std::path::Path::new(a)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();

    argv0 == "csm-hook"
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
    /// The csm-global `--profile <name>` that preceded the subcommand word,
    /// if any. Always `None` for the implicit `run` fallthrough, where the
    /// `--profile` tokens stay in `rest` for `cli::parser` to consume.
    pub(crate) profile: Option<String>,
}

impl Dispatch {
    /// A dispatch with no csm-global `--profile`.
    fn bare(subcommand: &'static str, rest_len: usize) -> Self {
        Dispatch {
            subcommand,
            rest_len,
            profile: None,
        }
    }
}

/// argv[0]-aware dispatch: which subcommand word to route to, how many
/// trailing args belong to it, and the csm-global `--profile` that preceded
/// the word.
///
/// - `csm-hook` argv[0] (symlink/rename form) → `hook` with everything after
///   argv[0] as `rest` (the alias takes no csm-global flags).
/// - Leading `--profile <name>` / `--profile=<name>` pairs are peeled off
///   (repeatable, last wins). If a reserved word follows, that word is
///   dispatched with `rest` = the tokens after it and the peeled name is
///   returned — this is what makes `csm --profile work statusline` run the
///   statusline under `work` instead of forwarding `statusline` to claude as
///   a prompt.
/// - Otherwise `args[1]` matched against `CSM_RESERVED_SUBCOMMANDS` → that
///   word with `rest` = `args[2..]`.
/// - Anything else — a bare `csm`, a dangling `--profile`, or a non-reserved
///   token like `csm --profile work -p 'hi'` — falls through to the implicit
///   `run` with `rest` = EVERYTHING after argv[0], `--profile` tokens
///   included, and `profile: None`. `csm run`'s own parser then handles that
///   form exactly as it always has.
pub(crate) fn dispatch_subcommand(args: &[OsString]) -> Dispatch {
    if invoked_as_hook_alias(args) {
        return Dispatch::bare("hook", args.len().saturating_sub(1));
    }

    let (idx, profile) = peel_global_profile(args);

    if let Some(word) = args
        .get(idx)
        .and_then(|a| reserved_word(a.to_string_lossy().as_ref()))
    {
        return Dispatch {
            subcommand: word,
            rest_len: args.len() - idx - 1,
            profile,
        };
    }

    Dispatch::bare("run", args.len().saturating_sub(1))
}

/// The `CSM_RESERVED_SUBCOMMANDS` entry equal to `candidate`, if any.
fn reserved_word(candidate: &str) -> Option<&'static str> {
    CSM_RESERVED_SUBCOMMANDS
        .iter()
        .copied()
        .find(|w| *w == candidate)
}

/// Peel leading csm-global `--profile <name>` / `--profile=<name>` pairs off
/// `args[1..]`, last one winning. Returns the index of the first token that is
/// not part of such a pair, plus the peeled name.
///
/// Only `--profile` is global: `-p` is claude's own print flag and `-P` is not
/// csm's, so neither is ever consumed here.
///
/// A dangling `--profile` — no value at all, or a dash-prefixed next token,
/// the same guard `cli::parser::consume_required_value` applies — stops the
/// peel AT that token, so the caller falls through to the implicit `run` and
/// the run parser sees the arguments exactly as typed.
fn peel_global_profile(args: &[OsString]) -> (usize, Option<String>) {
    let mut idx = 1;
    let mut profile: Option<String> = None;

    while idx < args.len() {
        let token = args[idx].to_string_lossy();
        if let Some(value) = token.strip_prefix("--profile=") {
            if value.is_empty() {
                break;
            }
            profile = Some(value.to_owned());
            idx += 1;
        } else if token == "--profile" {
            match args.get(idx + 1).map(|v| v.to_string_lossy()) {
                Some(value) if !value.is_empty() && !value.starts_with('-') => {
                    profile = Some(value.into_owned());
                    idx += 2;
                }
                _ => break,
            }
        } else {
            break;
        }
    }

    (idx, profile)
}

/// Rebuild `csm run`'s argument list when a csm-global `--profile` preceded an
/// explicit `run` word (`csm --profile work run -c`).
///
/// The flag is re-injected in front of run's own args so the pin travels
/// through the SAME `cli::parser` `--profile` path as `csm run --profile work
/// -c` (explicit choice, skip all picking) instead of being silently dropped.
pub(crate) fn run_args_with_profile(profile: Option<&str>, rest: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(rest.len() + 2);
    if let Some(name) = profile {
        out.push(OsString::from("--profile"));
        out.push(OsString::from(name));
    }
    out.extend_from_slice(rest);
    out
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
    fn invoked_as_hook_alias_true_for_csm_hook_argv0() {
        let a = vec![OsString::from("csm-hook"), OsString::from("--version")];
        assert!(invoked_as_hook_alias(&a));
    }

    #[test]
    fn invoked_as_hook_alias_false_for_plain_csm() {
        let a = vec![OsString::from("csm"), OsString::from("--version")];
        assert!(!invoked_as_hook_alias(&a));
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

    /// Dispatch `ss` and return `(subcommand, rest, profile)` — `rest` sliced
    /// exactly the way `main()` slices it, as plain strings for comparison.
    fn routed(ss: &[&str]) -> (&'static str, Vec<String>, Option<String>) {
        let a = argv(ss);
        let d = dispatch_subcommand(&a);
        let rest: Vec<String> = a[a.len() - d.rest_len..]
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        (d.subcommand, rest, d.profile)
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
    fn dispatch_explicit_pick_account() {
        let a = argv(&["csm", "pick-account", "home", "--include-current"]);
        assert_eq!(dispatch_subcommand(&a).subcommand, "pick-account");
    }

    #[test]
    fn dispatch_explicit_profiles() {
        let a = argv(&["csm", "profiles", "list"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "profiles");
        assert_eq!(d.rest_len, 1);
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
        assert_eq!(d.profile, None);
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
    fn dispatch_explicit_current_usage() {
        let a = argv(&["csm", "current-usage", "home"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "current-usage");
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
        let a = argv(&["csm", "run", "-c", "--profile=work"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "run");
        assert_eq!(d.rest_len, 2);
    }

    // ── csm-global `--profile` before the subcommand word ─────────────────────
    //
    // The bug (issue #25): `csm --profile home statusline` used to dispatch as
    // an implicit `run`, run's parser ate `--profile home`, and `statusline`
    // reached claude as a PROMPT — a full session instead of a status line.

    #[test]
    fn dispatch_global_profile_then_reserved_word() {
        assert_eq!(
            routed(&["csm", "--profile", "home", "statusline"]),
            ("statusline", vec![], Some("home".to_owned()))
        );
    }

    #[test]
    fn dispatch_global_profile_equals_form_keeps_subcommand_args() {
        assert_eq!(
            routed(&["csm", "--profile=home", "usage", "--json"]),
            ("usage", vec!["--json".to_owned()], Some("home".to_owned()))
        );
    }

    #[test]
    fn dispatch_global_profile_before_claude_passthrough() {
        assert_eq!(
            routed(&["csm", "--profile", "home", "claude", "mcp", "list"]),
            (
                "claude",
                vec!["mcp".to_owned(), "list".to_owned()],
                Some("home".to_owned())
            )
        );
    }

    /// No reserved word follows, so this is an ordinary launch: `run` gets
    /// EVERY token back (the `--profile` pair included) and dispatch reports
    /// no global profile, leaving `cli::parser` to pin it exactly as before.
    #[test]
    fn dispatch_global_profile_then_non_reserved_token_is_unchanged_run() {
        assert_eq!(
            routed(&["csm", "--profile", "home", "-p", "hi"]),
            (
                "run",
                vec![
                    "--profile".to_owned(),
                    "home".to_owned(),
                    "-p".to_owned(),
                    "hi".to_owned()
                ],
                None
            )
        );
    }

    #[test]
    fn dispatch_dangling_global_profile_is_run() {
        assert_eq!(
            routed(&["csm", "--profile"]),
            ("run", vec!["--profile".to_owned()], None)
        );
    }

    /// A dash-prefixed value is not a profile name (same guard the run parser
    /// applies), so the whole thing is an ordinary launch.
    #[test]
    fn dispatch_global_profile_with_flag_value_is_run() {
        assert_eq!(
            routed(&["csm", "--profile", "--interactive", "statusline"]),
            (
                "run",
                vec![
                    "--profile".to_owned(),
                    "--interactive".to_owned(),
                    "statusline".to_owned()
                ],
                None
            )
        );
    }

    #[test]
    fn dispatch_repeated_global_profile_last_wins() {
        assert_eq!(
            routed(&[
                "csm",
                "--profile",
                "home",
                "--profile",
                "work",
                "statusline"
            ]),
            ("statusline", vec![], Some("work".to_owned()))
        );
    }

    /// `-p` is claude's print flag and `-P` is nobody's — neither is a csm
    /// global, so both stay ordinary `run` tokens.
    #[test]
    fn dispatch_short_p_flags_are_not_global_profile() {
        for flag in ["-p", "-P"] {
            let (cmd, rest, profile) = routed(&["csm", flag, "home", "statusline"]);
            assert_eq!(cmd, "run", "`csm {flag} …` must stay an implicit run");
            assert_eq!(rest.len(), 3);
            assert_eq!(profile, None);
        }
    }

    #[test]
    fn dispatch_global_profile_before_explicit_run_word() {
        assert_eq!(
            routed(&["csm", "--profile", "home", "run", "-c"]),
            ("run", vec!["-c".to_owned()], Some("home".to_owned()))
        );
    }

    // ── run_args_with_profile ─────────────────────────────────────────────────

    #[test]
    fn run_args_with_profile_reinjects_the_flag() {
        let rest = argv(&["-c", "--model", "claude-x-1"]);
        assert_eq!(
            run_args_with_profile(Some("work"), &rest),
            argv(&["--profile", "work", "-c", "--model", "claude-x-1"])
        );
    }

    #[test]
    fn run_args_with_profile_none_is_rest_unchanged() {
        let rest = argv(&["-c", "hello"]);
        assert_eq!(run_args_with_profile(None, &rest), rest);
    }

    // ── argv[0]-aware hook dispatch ───────────────────────────────────────────

    #[test]
    fn dispatch_argv0_csm_hook_routes_to_hook() {
        let a = argv(&["csm-hook", "--owner", "/tmp/dir"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "hook");
        assert_eq!(d.rest_len, 2);
        assert_eq!(d.profile, None);
    }

    #[test]
    fn dispatch_argv0_csm_hook_no_args() {
        let a = argv(&["csm-hook"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "hook");
        assert_eq!(d.rest_len, 0);
    }

    /// The alias never peels csm globals: `csm-hook --profile home` is the
    /// hook with those two tokens as its own args, not a profile pin.
    #[test]
    fn dispatch_argv0_csm_hook_ignores_global_profile() {
        let a = argv(&["csm-hook", "--profile", "home"]);
        let d = dispatch_subcommand(&a);
        assert_eq!(d.subcommand, "hook");
        assert_eq!(d.rest_len, 2);
        assert_eq!(d.profile, None);
    }
}
