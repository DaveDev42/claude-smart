//! Single source of truth for the two disjoint reserved-subcommand-word sets
//! and the argv[0]-aware dispatch rule that picks between them.
//!
//! `CSM_RESERVED_SUBCOMMANDS` is the exact word list `main()` used to match
//! inline at `args[1]`; `CLAUDE_RESERVED_SUBCOMMANDS` exists only so tests can
//! assert the two sets stay disjoint (CLAUDE.md invariant 2 — csm must never
//! collide with a claude subcommand). Any word not in the csm set falls
//! through to an implicit `csm run` and reaches `claude` verbatim.

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
];

/// `claude`'s own reserved subcommand words. Never matched against by
/// `dispatch_subcommand` — this list exists so
/// `csm_and_claude_reserved_sets_are_disjoint` can assert the two sets never
/// overlap, so it is otherwise dead outside `#[cfg(test)]`.
#[allow(dead_code)]
pub(crate) const CLAUDE_RESERVED_SUBCOMMANDS: &[&str] = &[
    "agents",
    "auth",
    "auto-mode",
    "doctor",
    "install",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "setup-token",
    "ultrareview",
    "update",
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

/// argv[0]-aware dispatch: which subcommand word to route to, and how many
/// trailing args belong to it (always `args.len() - <words consumed>`).
///
/// - `csm-hook` argv[0] (symlink/rename form) → `("hook", len - 1)`.
/// - Otherwise `args[1]` matched against `CSM_RESERVED_SUBCOMMANDS` →
///   `(word, len - 2)`.
/// - Anything else, including a bare `csm` → implicit `("run", len - 1)`.
pub(crate) fn dispatch_subcommand(args: &[OsString]) -> (&'static str, usize) {
    if invoked_as_hook_alias(args) {
        return ("hook", args.len() - 1);
    }

    if args.len() >= 2 {
        let candidate = args[1].to_string_lossy();
        if let Some(word) = CSM_RESERVED_SUBCOMMANDS
            .iter()
            .find(|w| **w == candidate.as_ref())
        {
            return (word, args.len() - 2);
        }
    }

    ("run", args.len() - 1)
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
}
