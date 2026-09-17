//! Which of a launch's claude flags survive a limit-switch relaunch.
//!
//! `csm run` forwards every argument it does not consume itself to claude
//! verbatim (`cli::parser`'s `passthru`) and remembers that list in the
//! session's sidecar. When a limit switch relaunches the session under another
//! profile, the replacement claude must come back up shaped the way the user
//! launched it: a session started as `csm --dangerously-skip-permissions
//! --add-dir /x` that returns without those two stalls on a permission prompt
//! with nobody watching it.
//!
//! Replaying the remembered list verbatim is wrong, though. The hop builds its
//! own argv — `--resume <sid>`, the sidecar's mode/effort/model, a handoff
//! prompt — so the session verbs (`--resume`, `--continue`, `--session-id`),
//! the one-shot modes (`--print` and its format flags) and the initial prompt
//! would either fight that argv or replay a turn the resumed conversation
//! already holds.
//!
//! [`carry_passthru`] is the allow-list that decides, and it is pure: no I/O,
//! no clock, no environment. It knows each carried flag's arity so a value
//! travels with its flag, and it drops anything not on a list — including a
//! flag this version of csm has never heard of, because guessing an unknown
//! flag's arity would either swallow the following token or strand a value on
//! the argv. Dropped tokens come back to the caller so the relaunch can record
//! what it left behind.
//!
//! The spellings are claude's own (`claude --help`, Claude Code 2.1.x).

use std::ffi::OsString;

/// The outcome of filtering one launch's passthru for a relaunch hop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Carried {
    /// Tokens to append to the hop's argv, in their original order.
    pub carried: Vec<OsString>,
    /// Tokens left behind, in their original order.
    pub dropped: Vec<String>,
    /// True when the last carried token is a value of a variadic flag, so
    /// claude's parser would keep collecting whatever comes next. The caller
    /// must close the run with `--` before appending a handoff prompt, or the
    /// prompt becomes one more directory (or tool, or config) and the resumed
    /// session comes back with no first turn.
    pub trailing_variadic: bool,
}

/// Flags that take no value — carried on their own.
const BOOLEAN: &[&str] = &[
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--strict-mcp-config",
    "--verbose",
    "--chrome",
    "--no-chrome",
    "--ide",
    "--bare",
    "--restricted",
    "--safe-mode",
    "--disable-slash-commands",
    "--ax-screen-reader",
    "--brief",
    "--exclude-dynamic-system-prompt-sections",
];

/// Flags that take exactly one value — carried with that value.
const ONE_VALUE: &[&str] = &[
    "--settings",
    "--setting-sources",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--system-prompt",
    "--system-prompt-file",
    "--system-prompt-snapshot",
    "--agent",
    "--agents",
    "--plugin-dir",
    "--plugin-url",
    "--fallback-model",
    "--autocompact",
    "--max-budget-usd",
    "--debug-file",
    "-n",
    "--name",
];

/// Flags that take one or more values — carried with every value token up to
/// the next flag.
const VARIADIC: &[&str] = &[
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--mcp-config",
    "--tools",
    "--betas",
    "--file",
];

/// Never carried, and each one takes a value that has to go with it — dropping
/// the flag alone would leave its value behind as a stray positional, which
/// claude would read as a prompt.
///
/// `--permission-mode`, `--effort` and `--model` are on this list defensively:
/// `csm run`'s own parser consumes them, so they never reach passthru, and the
/// hop re-applies them from the sidecar instead (`Sidecar::sidecar_flags`).
const NEVER_WITH_VALUE: &[&str] = &[
    "--session-id",
    "--output-format",
    "--input-format",
    "--remote-control-session-name-prefix",
    "--permission-mode",
    "--effort",
    "--model",
    "--permission-prompts",
    "--json-schema",
];

/// Split a token into its flag name and whether it carried an inline value.
///
/// `--settings=x` → `("--settings", true)`; `--settings` → `("--settings",
/// false)`. A positional that happens to contain `=` (`KEY=value`) is not a
/// flag, so it keeps its whole text as the "name" and matches no list.
fn split_flag(token: &str) -> (&str, bool) {
    match token.split_once('=') {
        Some((name, _)) if name.starts_with('-') => (name, true),
        _ => (token, false),
    }
}

/// True when `passthru[i]` exists and is a value rather than the next flag.
fn value_at(passthru: &[String], i: usize) -> Option<&String> {
    passthru.get(i).filter(|v| !v.starts_with('-'))
}

/// Select the tokens of `passthru` that are safe to replay on a
/// `claude --resume` relaunch, and report the ones that were not.
pub fn carry_passthru(passthru: &[String]) -> Carried {
    let mut out = Carried::default();
    let mut i = 0;

    while i < passthru.len() {
        let token = passthru[i].as_str();

        // A bare `--` ends claude's flag parsing: the rest is prompt text.
        if token == "--" {
            out.dropped.extend(passthru[i..].iter().cloned());
            break;
        }

        let (name, inline_value) = split_flag(token);

        if BOOLEAN.contains(&name) {
            out.carried.push(OsString::from(token));
            out.trailing_variadic = false;
            i += 1;
        } else if ONE_VALUE.contains(&name) {
            if inline_value {
                out.carried.push(OsString::from(token));
                out.trailing_variadic = false;
                i += 1;
            } else if let Some(value) = value_at(passthru, i + 1) {
                out.carried.push(OsString::from(token));
                out.carried.push(OsString::from(value));
                out.trailing_variadic = false;
                i += 2;
            } else {
                // No value followed. Carrying the flag alone would make claude
                // read the handoff prompt as its value.
                out.dropped.push(token.to_owned());
                i += 1;
            }
        } else if VARIADIC.contains(&name) {
            if inline_value {
                // `--add-dir=/a` is one token: claude reads the value from the
                // token itself and collects nothing further.
                out.carried.push(OsString::from(token));
                out.trailing_variadic = false;
                i += 1;
            } else {
                let mut end = i + 1;
                while value_at(passthru, end).is_some() {
                    end += 1;
                }
                if end == i + 1 {
                    out.dropped.push(token.to_owned());
                } else {
                    out.carried.push(OsString::from(token));
                    out.carried
                        .extend(passthru[i + 1..end].iter().map(OsString::from));
                    out.trailing_variadic = true;
                }
                i = end;
            }
        } else if NEVER_WITH_VALUE.contains(&name) {
            out.dropped.push(token.to_owned());
            i += 1;
            if !inline_value && let Some(value) = value_at(passthru, i) {
                out.dropped.push(value.clone());
                i += 1;
            }
        } else {
            // Everything else goes: the valueless never-carry flags (`--resume`,
            // `--continue`, `--print`, `-v`, `--bg`, `--worktree`, `--tmux` and
            // the rest), a flag no list knows (unknown arity, so consuming the
            // next token would be a guess), and the positional prompt — which
            // `--resume` has already replayed as conversation.
            out.dropped.push(token.to_owned());
            i += 1;
        }
    }

    out
}

/// Render a dropped-token list for the limit-switch log.
///
/// Flags are named; everything else is counted, never quoted. A dropped run
/// holds the launch's initial prompt and the values of flags like
/// `--session-id`, and that log is the one place a switch is explained to a
/// human — it has never carried conversation text and must not start now.
///
/// Only the unix relaunch loop logs, so this is dead on Windows until that
/// loop's gate lifts (same reason `build_next_cli` carries the attribute).
#[cfg_attr(windows, allow(dead_code))]
pub fn describe_dropped(dropped: &[String]) -> String {
    let (flags, rest): (Vec<&String>, Vec<&String>) = dropped
        .iter()
        .partition(|t| t.starts_with('-') && *t != "--");
    let named = flags
        .iter()
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    match (named.is_empty(), rest.len()) {
        (true, n) => format!("{n} unnamed token(s)"),
        (false, 0) => named,
        (false, n) => format!("{named} +{n} unnamed token(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the selection over a `&str` argv, the way a launch's passthru reads.
    fn carry(args: &[&str]) -> (Vec<String>, Vec<String>) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let out = carry_passthru(&owned);
        let carried = out
            .carried
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        (carried, out.dropped)
    }

    #[test]
    fn empty_input_carries_and_drops_nothing() {
        let (carried, dropped) = carry(&[]);
        assert!(carried.is_empty());
        assert!(dropped.is_empty());
    }

    // ── boolean flags ─────────────────────────────────────────────────────────

    #[test]
    fn boolean_flags_are_carried_as_is() {
        // The one that matters most: an unattended session that loses
        // --dangerously-skip-permissions stalls on the first permission prompt.
        let (carried, dropped) = carry(&["--dangerously-skip-permissions", "--verbose"]);
        assert_eq!(carried, ["--dangerously-skip-permissions", "--verbose"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn every_boolean_flag_is_carried() {
        for flag in BOOLEAN {
            let (carried, dropped) = carry(&[flag]);
            assert_eq!(carried, [flag.to_string()], "{flag} should be carried");
            assert!(dropped.is_empty(), "{flag} should not be dropped");
        }
    }

    // ── one-value flags ───────────────────────────────────────────────────────

    #[test]
    fn one_value_flag_separate_form_carries_its_value() {
        let (carried, dropped) = carry(&["--settings", "/Users/example/s.json"]);
        assert_eq!(carried, ["--settings", "/Users/example/s.json"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn one_value_flag_equals_form_carries_the_whole_token() {
        let (carried, dropped) = carry(&["--settings=/Users/example/s.json"]);
        assert_eq!(carried, ["--settings=/Users/example/s.json"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn every_one_value_flag_is_carried_with_its_value() {
        for flag in ONE_VALUE {
            let (carried, dropped) = carry(&[flag, "v"]);
            assert_eq!(carried, [flag.to_string(), "v".to_owned()], "{flag}");
            assert!(dropped.is_empty(), "{flag}");
        }
    }

    #[test]
    fn one_value_flag_without_a_value_is_dropped() {
        // Carrying it alone would make claude swallow the handoff prompt as the
        // flag's value.
        let (carried, dropped) = carry(&["--settings"]);
        assert!(carried.is_empty());
        assert_eq!(dropped, ["--settings"]);
    }

    #[test]
    fn one_value_flag_followed_by_a_flag_is_dropped_but_the_flag_survives() {
        let (carried, dropped) = carry(&["--settings", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--settings"]);
    }

    // ── variadic flags ────────────────────────────────────────────────────────

    #[test]
    fn variadic_flag_takes_every_value_up_to_the_next_flag() {
        let (carried, dropped) = carry(&["--add-dir", "/a", "/b", "--verbose", "/c"]);
        assert_eq!(carried, ["--add-dir", "/a", "/b", "--verbose"]);
        // `/c` is a positional once `--verbose` has closed the value run.
        assert_eq!(dropped, ["/c"]);
    }

    // ── closing an open variadic run ───────────────────────────────────────
    // `carry_passthru` reports whether the tokens it hands back end inside a
    // variadic flag. `build_next_cli` uses it to decide whether the handoff
    // prompt needs a `--` in front of it.

    #[test]
    fn trailing_variadic_is_set_when_the_last_carried_token_is_a_variadic_value() {
        let owned = ["--add-dir".to_owned(), "/a".to_owned()];
        assert!(carry_passthru(&owned).trailing_variadic);
    }

    #[test]
    fn trailing_variadic_is_clear_after_a_boolean_closes_the_run() {
        let owned = [
            "--add-dir".to_owned(),
            "/a".to_owned(),
            "--verbose".to_owned(),
        ];
        assert!(!carry_passthru(&owned).trailing_variadic);
    }

    #[test]
    fn trailing_variadic_is_clear_for_an_inline_variadic_value() {
        // `--add-dir=/a` carries its value in the token, so claude collects
        // nothing after it.
        let owned = ["--add-dir=/a".to_owned()];
        let out = carry_passthru(&owned);
        assert_eq!(out.carried, ["--add-dir=/a"]);
        assert!(!out.trailing_variadic);
    }

    #[test]
    fn trailing_variadic_is_clear_when_nothing_is_carried() {
        assert!(!carry_passthru(&[]).trailing_variadic);
        let owned = ["do the thing".to_owned()];
        assert!(!carry_passthru(&owned).trailing_variadic);
    }

    #[test]
    fn trailing_variadic_is_clear_when_the_variadic_had_no_values_to_carry() {
        // The flag is dropped whole, so nothing it could absorb was emitted.
        let owned = ["--add-dir".to_owned()];
        let out = carry_passthru(&owned);
        assert!(out.carried.is_empty());
        assert!(!out.trailing_variadic);
    }

    // ── describe_dropped: names flags, counts everything else ──────────────

    #[test]
    fn describe_dropped_names_flags_and_counts_the_rest() {
        let dropped = vec![
            "--print".to_owned(),
            "--session-id".to_owned(),
            "11111111-2222-3333-4444-555555555555".to_owned(),
            "do the thing".to_owned(),
        ];
        assert_eq!(
            describe_dropped(&dropped),
            "--print --session-id +2 unnamed token(s)"
        );
    }

    #[test]
    fn describe_dropped_never_quotes_prompt_text() {
        // The limit-switch log explains a switch to a human and has never held
        // conversation text. A dropped prompt is a count, not a quote.
        let dropped = vec!["remember the passphrase hunter2".to_owned()];
        let rendered = describe_dropped(&dropped);
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(rendered, "1 unnamed token(s)");
    }

    #[test]
    fn describe_dropped_of_flags_only_is_just_the_flags() {
        let dropped = vec!["--print".to_owned(), "--continue".to_owned()];
        assert_eq!(describe_dropped(&dropped), "--print --continue");
    }

    #[test]
    fn describe_dropped_counts_a_bare_separator_rather_than_naming_it() {
        // `--` is where prompt text starts, so it is not a flag name worth
        // printing; it and everything after it are counted.
        let dropped = vec!["--".to_owned(), "the prompt".to_owned()];
        assert_eq!(describe_dropped(&dropped), "2 unnamed token(s)");
    }

    #[test]
    fn variadic_flag_absorbs_a_prompt_that_directly_follows_its_values() {
        // Not a bug to fix here: claude's own parser read the original launch
        // the same way, so `csm --add-dir /a "do the thing"` never started with
        // a prompt in the first place. Replaying the launch faithfully means
        // replaying that, and the alternative — guessing that the last value of
        // a run is really a prompt — would drop a legitimate second directory.
        let (carried, dropped) = carry(&["--add-dir", "/a", "do the thing"]);
        assert_eq!(carried, ["--add-dir", "/a", "do the thing"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn variadic_flag_equals_form_carries_the_whole_token() {
        let (carried, dropped) = carry(&["--add-dir=/a"]);
        assert_eq!(carried, ["--add-dir=/a"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn variadic_flag_runs_to_the_end_of_the_argv() {
        let (carried, dropped) = carry(&["--mcp-config", "a.json", "b.json"]);
        assert_eq!(carried, ["--mcp-config", "a.json", "b.json"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn variadic_flag_without_values_is_dropped() {
        let (carried, dropped) = carry(&["--allowedTools", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--allowedTools"]);
    }

    #[test]
    fn every_variadic_flag_is_carried_with_its_values() {
        for flag in VARIADIC {
            let (carried, dropped) = carry(&[flag, "one", "two"]);
            assert_eq!(
                carried,
                [flag.to_string(), "one".to_owned(), "two".to_owned()],
                "{flag}"
            );
            assert!(dropped.is_empty(), "{flag}");
        }
    }

    // ── never carried ─────────────────────────────────────────────────────────

    #[test]
    fn never_carried_flag_takes_its_value_down_with_it() {
        let (carried, dropped) = carry(&["--output-format", "json"]);
        assert!(carried.is_empty());
        assert_eq!(dropped, ["--output-format", "json"]);
    }

    #[test]
    fn never_carried_flag_equals_form_is_dropped_whole() {
        let (carried, dropped) = carry(&["--output-format=json", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--output-format=json"]);
    }

    #[test]
    fn never_carried_flag_missing_its_value_drops_only_itself() {
        let (carried, dropped) = carry(&["--session-id", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--session-id"]);
    }

    #[test]
    fn every_valueless_never_carried_flag_is_dropped() {
        // The hop supplies its own session verb and runs interactively; these
        // would fight it. Listed here (rather than in a const the selection
        // reads) because the fall-through drops them for the same reason it
        // drops an unknown flag.
        const NEVER_BARE: &[&str] = &[
            "-r",
            "--resume",
            "-c",
            "--continue",
            "--fork-session",
            "-p",
            "--print",
            "--include-partial-messages",
            "--include-hook-events",
            "--replay-user-messages",
            "--forward-subagent-text",
            "-v",
            "--version",
            "-h",
            "--help",
            "--bg",
            "--background",
            "--cloud",
            "--environment",
            "--from-pr",
            "--teleport",
            "--remote-control",
            "--tmux",
            "-w",
            "--worktree",
            "-d",
            "--debug",
            "--prompt-suggestions",
            "--no-session-persistence",
        ];
        for flag in NEVER_BARE {
            let (carried, dropped) = carry(&[flag]);
            assert!(carried.is_empty(), "{flag} must not be carried");
            assert_eq!(dropped, [flag.to_string()], "{flag}");
        }
    }

    #[test]
    fn model_effort_and_permission_mode_are_dropped_defensively() {
        // csm's own parser eats these before they can reach passthru; the hop
        // re-applies them from the sidecar. If one ever does show up here it
        // must not reach the argv twice.
        let (carried, dropped) = carry(&["--model", "some-model", "--effort", "high"]);
        assert!(carried.is_empty());
        assert_eq!(dropped, ["--model", "some-model", "--effort", "high"]);
    }

    // ── unknown flags and positionals ─────────────────────────────────────────

    #[test]
    fn unknown_flag_is_dropped_without_consuming_the_next_token() {
        // Unknown arity: the token after it might be its value or a positional,
        // and carrying the flag on a guess is worse than losing it.
        let (carried, dropped) = carry(&["--brand-new-flag", "maybe-a-value", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--brand-new-flag", "maybe-a-value"]);
    }

    #[test]
    fn positional_prompt_before_flags_is_dropped() {
        let (carried, dropped) = carry(&["do the thing", "--verbose"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["do the thing"]);
    }

    #[test]
    fn positional_prompt_after_flags_is_dropped() {
        let (carried, dropped) = carry(&["--verbose", "do the thing"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["do the thing"]);
    }

    #[test]
    fn a_positional_containing_an_equals_sign_is_not_read_as_a_flag() {
        let (carried, dropped) = carry(&["KEY=value"]);
        assert!(carried.is_empty());
        assert_eq!(dropped, ["KEY=value"]);
    }

    // ── `--` terminator ───────────────────────────────────────────────────────

    #[test]
    fn double_dash_stops_the_scan_and_drops_the_rest() {
        let (carried, dropped) = carry(&["--verbose", "--", "--add-dir", "/a"]);
        assert_eq!(carried, ["--verbose"]);
        assert_eq!(dropped, ["--", "--add-dir", "/a"]);
    }

    // ── a realistic launch ────────────────────────────────────────────────────

    #[test]
    fn mixed_argv_keeps_session_shape_and_drops_the_rest() {
        let (carried, dropped) = carry(&[
            "--dangerously-skip-permissions",
            "--add-dir",
            "/Users/example/a",
            "/Users/example/b",
            "--settings=/Users/example/s.json",
            "--continue",
            "--output-format",
            "json",
            "--allowed-tools",
            "Bash",
            "Read",
            "--unknown-thing",
            "do the thing",
        ]);
        assert_eq!(
            carried,
            [
                "--dangerously-skip-permissions",
                "--add-dir",
                "/Users/example/a",
                "/Users/example/b",
                "--settings=/Users/example/s.json",
                "--allowed-tools",
                "Bash",
                "Read",
            ],
            "carried order must match the launch order"
        );
        assert_eq!(
            dropped,
            [
                "--continue",
                "--output-format",
                "json",
                "--unknown-thing",
                "do the thing",
            ]
        );
    }
}
