//! Hand-rolled positional flag parser for `csm run`.
//!
//! **Why not `clap`?** Claude's own flags (`-c`, `-r`, `--model`, `--effort`,
//! …) must be forwarded verbatim to `claude`. A standard clap binding would
//! swallow them. This parser consumes only the flags that `csm` itself cares
//! about; everything else accumulates in `passthru`.
//!
//! Handles arg parsing in full:
//! - Consumed-internally flags: `-i`/`--interactive`, `-n`/`--new`,
//!   `-c`/`--continue`, `-A`/`--pick-account`, `--no-pick`, `-r`/`--resume`,
//!   `--permission-mode`, `--effort`, `--model`, `--session-id`, `--profile`.
//! - `-h`/`--help` **only while nothing has been forwarded yet** → run's own
//!   help. Once a passthru token exists (`csm run -p --help`) or the `--`
//!   boundary has been crossed (`csm run -- --help`), it is claude's flag and
//!   forwards verbatim.
//! - **Equals-form:** `--resume=<id>`, `--permission-mode=<m>`,
//!   `--effort=<e>`, `--model=<m>`, `--session-id=<id>`, `--profile=<p>`.
//! - **`-r`/`--resume` alias resolution:** non-UUID value → alias token;
//!   missing / dash-prefixed next token → promote to picker ([`ResumeArg::Picker`]).
//! - `--` stops parsing; everything after goes verbatim into `passthru`.
//! - Unknown flags / positional args go into `passthru`.
//!
//! Matches the legacy shell implementation's `while (( $# )); do … done`
//! arg-parse block.

use std::ffi::OsString;

// ─── types ────────────────────────────────────────────────────────────────────

/// The resolved intent of `-r`/`--resume`.
///
/// - `Id(s)` — a concrete session id (UUID) or alias name to look up.
///   Alias-resolution (`resolve-alias`) is deferred to `session/alias.rs`; this
///   parser only captures the raw token.
/// - `Picker` — the flag was present but no id followed (next token absent or
///   dash-prefixed): open the interactive session picker.
///
/// Matches the legacy shell implementation's logic:
/// ```zsh
/// -r|--resume)
///   if [[ -n "${2:-}" && "$2" != -* ]]; then
///     resume_id="$2"; shift 2
///   else
///     want_picker=true; shift
///   fi ;;
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeArg {
    /// A concrete id string (UUID or alias).
    Id(String),
    /// No id supplied — open the interactive picker.
    Picker,
}

/// Flags consumed and interpreted by `csm run` itself.
///
/// `None` means the flag was absent; `Some(ResumeArg::Picker)` means "open
/// picker" (the flag was present but no value followed).
///
/// Reproduces the legacy shell implementation's local variables:
/// ```zsh
/// local want_picker=false want_continue=false pick_account=false no_pick=false
/// local want_new=false
/// local resume_id="" o_mode="" o_effort="" o_model="" o_session="" o_profile=""
/// ```
#[derive(Debug, Default, PartialEq)]
pub struct Flags {
    /// `-i` / `--interactive` — manual pick. Forces BOTH pickers: skips account
    /// auto-pick and always opens the recommendation-ordered account picker, and
    /// opens the session picker. `--profile <p>` still wins. (`want_picker=true`
    /// in the zsh source — which forced only the session picker.)
    pub interactive: bool,
    /// `-n` / `--new` — start a fresh session, skip the session picker.
    /// (`want_new=true` in the zsh source)
    pub new: bool,
    /// `-c` / `--continue` — continue the newest free session.
    /// (`want_continue=true` in the zsh source)
    pub continue_: bool,
    /// `-A` / `--pick-account` — force an account pick.
    /// (`pick_account=true` in the zsh source)
    pub pick_account: bool,
    /// `--no-pick` — suppress all automatic account picking.
    /// (`no_pick=true` in the zsh source)
    pub no_pick: bool,
    /// `-r` / `--resume [<id-or-alias>]`
    ///   - `None`                    → flag absent
    ///   - `Some(ResumeArg::Id(s))`  → value supplied (UUID or alias token)
    ///   - `Some(ResumeArg::Picker)` → flag present but no non-flag value followed
    ///
    /// Note: the parser does NOT resolve aliases here; that is `session/alias.rs`.
    pub resume: Option<ResumeArg>,
    /// `--permission-mode <m>` / `--permission-mode=<m>`
    /// (`o_mode` in the zsh source)
    pub permission_mode: Option<String>,
    /// `--effort <e>` / `--effort=<e>`
    /// (`o_effort` in the zsh source)
    pub effort: Option<String>,
    /// `--model <m>` / `--model=<m>`
    /// (`o_model` in the zsh source)
    pub model: Option<String>,
    /// `--session-id <id>` / `--session-id=<id>`
    /// (`o_session` in the zsh source)
    pub session_id: Option<String>,
    /// `--profile <p>` / `--profile=<p>`
    /// (`o_profile` in the zsh source)
    pub profile: Option<String>,
    /// `-h` / `--help` seen before anything was forwarded to claude — print
    /// `csm run`'s own usage and exit instead of launching.
    ///
    /// The window is deliberately narrow: only while `passthru` is still empty
    /// and before the `--` boundary. `csm run -- --help` and
    /// `csm run -p --help` are claude's help, not run's, so they forward.
    pub help: bool,
}

/// Result of parsing `csm run` arguments.
///
/// `passthru` is forwarded verbatim to `claude` as positional / extra
/// arguments.  Preserves `OsString` so non-UTF-8 paths survive unmodified.
#[derive(Debug, Default)]
pub struct ParsedArgs {
    /// Flags consumed internally by `csm run`.
    pub flags: Flags,
    /// Everything not consumed — forwarded verbatim to `claude`.
    pub passthru: Vec<OsString>,
}

// ─── public API ───────────────────────────────────────────────────────────────

/// Parse the arguments for `csm run` from a slice of OS strings.
///
/// The slice should be the arguments **after** the subcommand word has been
/// consumed by `main.rs`'s dispatcher (i.e. `args[2..]` or `args[1..]`
/// depending on dispatch mode).
///
/// Reproduces the legacy shell implementation's
/// `while (( $# )); do case "$1" in … esac; done` loop exactly, including:
/// - `--` passthrough terminator
/// - `-r`/`--resume` picker promotion
/// - equals-form via zsh's `${1#--flag=}` (Rust: `strip_prefix`)
/// - unrecognised args → `passthru`
pub fn parse(args: &[OsString]) -> ParsedArgs {
    let mut flags = Flags::default();
    let mut passthru: Vec<OsString> = Vec::new();

    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();

        // `--` stops csm-side parsing; everything after is passthru.
        if s == "--" {
            passthru.extend(iter.cloned());
            break;
        }

        // ── -h / --help, while nothing has been forwarded yet ──────────────────
        // `csm run --help` is a request for RUN's help; `csm run -p --help`
        // (help about a claude flag already on the line) and `csm run --
        // --help` are claude's, and fall through to passthru.
        if (s == "-h" || s == "--help") && passthru.is_empty() {
            flags.help = true;
            continue;
        }

        // ── no-value boolean flags ─────────────────────────────────────────────
        if s == "-i" || s == "--interactive" {
            flags.interactive = true;
            continue;
        }
        if s == "-n" || s == "--new" {
            flags.new = true;
            continue;
        }
        if s == "-c" || s == "--continue" {
            flags.continue_ = true;
            continue;
        }
        if s == "-A" || s == "--pick-account" {
            flags.pick_account = true;
            continue;
        }
        if s == "--no-pick" {
            flags.no_pick = true;
            continue;
        }

        // ── -r / --resume [<id-or-alias>] ─────────────────────────────────────
        // When the next token is absent or starts with '-', promote to the
        // picker instead of consuming it as the resume value.
        if s == "-r" || s == "--resume" {
            flags.resume = Some(consume_value_or_picker(&mut iter));
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--resume") {
            flags.resume = Some(ResumeArg::Id(val.to_owned()));
            continue;
        }

        // ── --permission-mode ──────────────────────────────────────────────────
        if s == "--permission-mode" {
            flags.permission_mode = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--permission-mode") {
            flags.permission_mode = Some(val.to_owned());
            continue;
        }

        // ── --effort ──────────────────────────────────────────────────────────
        if s == "--effort" {
            flags.effort = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--effort") {
            flags.effort = Some(val.to_owned());
            continue;
        }

        // ── --model ───────────────────────────────────────────────────────────
        if s == "--model" {
            flags.model = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--model") {
            flags.model = Some(val.to_owned());
            continue;
        }

        // ── --session-id ──────────────────────────────────────────────────────
        if s == "--session-id" {
            flags.session_id = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--session-id") {
            flags.session_id = Some(val.to_owned());
            continue;
        }

        // ── --profile ─────────────────────────────────────────────────────────
        if s == "--profile" {
            flags.profile = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--profile") {
            flags.profile = Some(val.to_owned());
            continue;
        }

        // Unrecognised argument: forward verbatim to claude.
        passthru.push(arg.clone());
    }

    ParsedArgs { flags, passthru }
}

// ─── internal helpers ─────────────────────────────────────────────────────────

/// Try to strip a `--flag=` prefix from `s`, returning the value slice.
/// Returns `None` if `s` does not start with `"{flag}="`.
///
/// Reproduces the legacy shell implementation's `${1#--flag=}` strip form.
fn strip_eq_prefix<'a>(s: &'a str, flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    s.strip_prefix(prefix.as_str())
}

/// Peek at the next argument. If it exists and does NOT start with `-`, consume
/// it and return `ResumeArg::Id(value)`. Otherwise return `ResumeArg::Picker`
/// (promote to picker intent) without advancing the iterator.
///
/// Reproduces the legacy shell implementation's `-r`/`--resume` guard:
/// ```zsh
/// if [[ -n "${2:-}" && "$2" != -* ]]; then
///   resume_id="$2"; shift 2
/// else
///   want_picker=true; shift
/// fi
/// ```
fn consume_value_or_picker(
    iter: &mut std::iter::Peekable<std::slice::Iter<'_, OsString>>,
) -> ResumeArg {
    match iter.peek() {
        Some(next) if !next.to_string_lossy().starts_with('-') => {
            ResumeArg::Id(iter.next().unwrap().to_string_lossy().into_owned())
        }
        _ => ResumeArg::Picker,
    }
}

/// Consume the next argument as a required value for a named flag.
/// If there is no next argument or the next starts with `-`, returns `None`
/// without advancing the iterator (leaving the next token for the main loop).
///
/// Used for `--permission-mode`, `--effort`, `--model`, `--session-id`,
/// `--profile`.  Mirrors zsh's `shift 2` form with the implicit "next token
/// must not be a flag" guard.
fn consume_required_value(
    iter: &mut std::iter::Peekable<std::slice::Iter<'_, OsString>>,
) -> Option<String> {
    match iter.peek() {
        Some(next) if !next.to_string_lossy().starts_with('-') => {
            Some(iter.next().unwrap().to_string_lossy().into_owned())
        }
        _ => None,
    }
}

// ─── test helpers ─────────────────────────────────────────────────────────────

/// Build a `Vec<OsString>` from string slices — test helper.
#[cfg(test)]
fn os_args(ss: &[&str]) -> Vec<OsString> {
    ss.iter().map(OsString::from).collect()
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ══════════════════════════════════════════════════════════════════════════
    // Boolean flags — short forms
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_interactive_short() {
        let r = parse(&os_args(&["-i"]));
        assert!(r.flags.interactive);
        assert!(!r.flags.continue_);
        assert!(!r.flags.pick_account);
        assert!(!r.flags.no_pick);
    }

    #[test]
    fn parse_new_short() {
        let r = parse(&os_args(&["-n"]));
        assert!(r.flags.new);
        assert!(!r.flags.interactive);
    }

    #[test]
    fn parse_continue_short() {
        let r = parse(&os_args(&["-c"]));
        assert!(r.flags.continue_);
        assert!(!r.flags.new);
    }

    #[test]
    fn parse_pick_account_short() {
        let r = parse(&os_args(&["-A"]));
        assert!(r.flags.pick_account);
    }

    #[test]
    fn parse_no_pick() {
        let r = parse(&os_args(&["--no-pick"]));
        assert!(r.flags.no_pick);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Boolean flags — long forms
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_interactive_long() {
        let r = parse(&os_args(&["--interactive"]));
        assert!(r.flags.interactive);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_new_long() {
        let r = parse(&os_args(&["--new"]));
        assert!(r.flags.new);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_continue_long() {
        let r = parse(&os_args(&["--continue"]));
        assert!(r.flags.continue_);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_pick_account_long() {
        let r = parse(&os_args(&["--pick-account"]));
        assert!(r.flags.pick_account);
        assert!(r.passthru.is_empty());
    }

    // ══════════════════════════════════════════════════════════════════════════
    // ResumeArg — space-separated forms
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_resume_short_with_uuid() {
        let r = parse(&os_args(&["-r", "01234567-89ab-cdef-0123-456789abcdef"]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "01234567-89ab-cdef-0123-456789abcdef".to_owned()
            ))
        );
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_long_with_uuid() {
        let r = parse(&os_args(&[
            "--resume",
            "01234567-89ab-cdef-0123-456789abcdef",
        ]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "01234567-89ab-cdef-0123-456789abcdef".to_owned()
            ))
        );
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_long_with_alias() {
        // Alias token: non-UUID, non-dash value — captured as Id, resolved later
        // by session/alias.rs (not here).
        let r = parse(&os_args(&["--resume", "my-session-alias"]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id("my-session-alias".to_owned()))
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // ResumeArg — equals-form
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_resume_equals_form_uuid() {
        let r = parse(&os_args(&["--resume=01234567-89ab-cdef-0123-456789abcdef"]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "01234567-89ab-cdef-0123-456789abcdef".to_owned()
            ))
        );
    }

    #[test]
    fn parse_resume_equals_form_alias() {
        let r = parse(&os_args(&["--resume=abc-def"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Id("abc-def".to_owned())));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // ResumeArg — picker promotion
    //
    // When the next token is absent or starts with '-', promote to the
    // picker (`ResumeArg::Picker`) instead of consuming it as the value.
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_resume_short_missing_value_promotes_to_picker() {
        // `-r` with no following argument → Picker
        let r = parse(&os_args(&["-r"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Picker));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_long_missing_value_promotes_to_picker() {
        // `--resume` with no following argument → Picker
        let r = parse(&os_args(&["--resume"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Picker));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_dash_prefixed_next_promotes_to_picker() {
        // Next token starts with '-' → promote to picker; the token stays in the
        // iterator and is parsed as its own flag (--model is consumed normally).
        let r = parse(&os_args(&["-r", "--model", "opus"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Picker));
        assert_eq!(r.flags.model.as_deref(), Some("opus"));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_long_dash_prefixed_next_promotes_to_picker() {
        // Same with the long form
        let r = parse(&os_args(&["--resume", "--effort", "high"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Picker));
        assert_eq!(r.flags.effort.as_deref(), Some("high"));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_resume_absent_means_none() {
        // No -r/--resume at all → None (not Picker)
        let r = parse(&os_args(&["-i"]));
        assert_eq!(r.flags.resume, None);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Value flags — space-separated forms
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_permission_mode_space() {
        let r = parse(&os_args(&["--permission-mode", "bypassPermissions"]));
        assert_eq!(
            r.flags.permission_mode.as_deref(),
            Some("bypassPermissions")
        );
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_effort_space() {
        let r = parse(&os_args(&["--effort", "high"]));
        assert_eq!(r.flags.effort.as_deref(), Some("high"));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_model_space() {
        let r = parse(&os_args(&["--model", "claude-opus-4-5"]));
        assert_eq!(r.flags.model.as_deref(), Some("claude-opus-4-5"));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_session_id_space() {
        let r = parse(&os_args(&[
            "--session-id",
            "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb",
        ]));
        assert_eq!(
            r.flags.session_id.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb")
        );
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn parse_profile_space() {
        let r = parse(&os_args(&["--profile", "home"]));
        assert_eq!(r.flags.profile.as_deref(), Some("home"));
        assert!(r.passthru.is_empty());
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Value flags — equals-form
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_permission_mode_equals() {
        let r = parse(&os_args(&["--permission-mode=bypassPermissions"]));
        assert_eq!(
            r.flags.permission_mode.as_deref(),
            Some("bypassPermissions")
        );
    }

    #[test]
    fn parse_effort_equals() {
        let r = parse(&os_args(&["--effort=high"]));
        assert_eq!(r.flags.effort.as_deref(), Some("high"));
    }

    #[test]
    fn parse_model_equals() {
        let r = parse(&os_args(&["--model=claude-opus-4-5"]));
        assert_eq!(r.flags.model.as_deref(), Some("claude-opus-4-5"));
    }

    #[test]
    fn parse_session_id_equals() {
        let r = parse(&os_args(&[
            "--session-id=aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb",
        ]));
        assert_eq!(
            r.flags.session_id.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb")
        );
    }

    #[test]
    fn parse_profile_equals() {
        let r = parse(&os_args(&["--profile=home"]));
        assert_eq!(r.flags.profile.as_deref(), Some("home"));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Space-form vs equals-form equivalence
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn permission_mode_space_and_equals_equivalent() {
        let r_space = parse(&os_args(&["--permission-mode", "plan"]));
        let r_eq = parse(&os_args(&["--permission-mode=plan"]));
        assert_eq!(r_space.flags.permission_mode, r_eq.flags.permission_mode);
    }

    #[test]
    fn effort_space_and_equals_equivalent() {
        let r_space = parse(&os_args(&["--effort", "xhigh"]));
        let r_eq = parse(&os_args(&["--effort=xhigh"]));
        assert_eq!(r_space.flags.effort, r_eq.flags.effort);
    }

    #[test]
    fn model_space_and_equals_equivalent() {
        let r_space = parse(&os_args(&["--model", "claude-sonnet-4-5"]));
        let r_eq = parse(&os_args(&["--model=claude-sonnet-4-5"]));
        assert_eq!(r_space.flags.model, r_eq.flags.model);
    }

    #[test]
    fn session_id_space_and_equals_equivalent() {
        let id = "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb";
        let r_space = parse(&os_args(&["--session-id", id]));
        let r_eq = parse(&os_args(&[&format!("--session-id={id}")]));
        assert_eq!(r_space.flags.session_id, r_eq.flags.session_id);
    }

    #[test]
    fn profile_space_and_equals_equivalent() {
        let r_space = parse(&os_args(&["--profile", "work"]));
        let r_eq = parse(&os_args(&["--profile=work"]));
        assert_eq!(r_space.flags.profile, r_eq.flags.profile);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // `--` passthrough terminator
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn double_dash_stops_parsing_passes_rest() {
        let r = parse(&os_args(&["-c", "--", "--model", "raw-arg"]));
        assert!(r.flags.continue_);
        // --model after -- is NOT consumed as a flag
        assert!(r.flags.model.is_none());
        assert_eq!(r.passthru, os_args(&["--model", "raw-arg"]));
    }

    #[test]
    fn double_dash_with_nothing_after() {
        let r = parse(&os_args(&["-i", "--"]));
        assert!(r.flags.interactive);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn double_dash_at_start() {
        // Everything (including csm-internal flags) ends up in passthru
        let r = parse(&os_args(&["--", "-c", "--model", "x"]));
        assert!(!r.flags.continue_);
        assert_eq!(r.passthru, os_args(&["-c", "--model", "x"]));
    }

    #[test]
    fn double_dash_preserves_multiple_args() {
        let r = parse(&os_args(&["--", "foo", "bar", "baz"]));
        assert_eq!(r.passthru, os_args(&["foo", "bar", "baz"]));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Passthru: claude's own flags fall through unchanged
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn passthru_unknown_long_flag() {
        let r = parse(&os_args(&["--dangerously-skip-permissions"]));
        assert_eq!(r.passthru, os_args(&["--dangerously-skip-permissions"]));
    }

    #[test]
    fn passthru_unknown_short_flag() {
        // e.g. claude's own -v or --version
        let r = parse(&os_args(&["-v"]));
        assert_eq!(r.passthru, os_args(&["-v"]));
    }

    #[test]
    fn passthru_positional_prompt() {
        // A plain prompt string (no leading dash) is a passthru positional
        let r = parse(&os_args(&["implement the feature"]));
        assert_eq!(r.passthru, os_args(&["implement the feature"]));
    }

    #[test]
    fn passthru_multiple_positional_args() {
        let r = parse(&os_args(&["foo", "bar", "baz"]));
        assert_eq!(r.passthru, os_args(&["foo", "bar", "baz"]));
    }

    #[test]
    fn passthru_claude_print_flag() {
        // --print is a claude flag not in csm's list
        let r = parse(&os_args(&["--print"]));
        assert_eq!(r.passthru, os_args(&["--print"]));
    }

    #[test]
    fn passthru_claude_output_format_flag() {
        // --output-format=json is a claude flag; passes through untouched
        let r = parse(&os_args(&["--output-format=json"]));
        assert_eq!(r.passthru, os_args(&["--output-format=json"]));
    }

    #[test]
    fn passthru_preserves_interleaving_with_csm_flags() {
        // csm flags and passthru can be interleaved (zsh's case statement does
        // exactly this — each unrecognised arg lands in passthru independently)
        let r = parse(&os_args(&[
            "--dangerously-skip-permissions",
            "-c",
            "my prompt",
        ]));
        assert!(r.flags.continue_);
        assert_eq!(
            r.passthru,
            os_args(&["--dangerously-skip-permissions", "my prompt"])
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Edge / degenerate cases
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn empty_args() {
        let r = parse(&[]);
        assert_eq!(r.flags, Flags::default());
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn all_boolean_flags_together() {
        let r = parse(&os_args(&["-i", "-c", "-A", "--no-pick"]));
        assert!(r.flags.interactive);
        assert!(r.flags.continue_);
        assert!(r.flags.pick_account);
        assert!(r.flags.no_pick);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn all_boolean_flags_long_forms() {
        let r = parse(&os_args(&[
            "--interactive",
            "--continue",
            "--pick-account",
            "--no-pick",
        ]));
        assert!(r.flags.interactive);
        assert!(r.flags.continue_);
        assert!(r.flags.pick_account);
        assert!(r.flags.no_pick);
    }

    /// Reproduces the real-world `csm -i -A` invocation from the legacy
    /// shell implementation's doc-comment: "pick a session AND the best
    /// account".
    #[test]
    fn interactive_plus_pick_account() {
        let r = parse(&os_args(&["-i", "-A"]));
        assert!(r.flags.interactive);
        assert!(r.flags.pick_account);
        assert!(!r.flags.no_pick);
        assert!(r.passthru.is_empty());
    }

    /// `csm --permission-mode plan --effort high` from the legacy shell doc.
    #[test]
    fn permission_mode_and_effort_fresh_session() {
        let r = parse(&os_args(&["--permission-mode", "plan", "--effort", "high"]));
        assert_eq!(r.flags.permission_mode.as_deref(), Some("plan"));
        assert_eq!(r.flags.effort.as_deref(), Some("high"));
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn combined_flags_passthru_and_double_dash() {
        let r = parse(&os_args(&[
            "-c",
            "--profile=work",
            "--effort",
            "low",
            "--",
            "extra",
        ]));
        assert!(r.flags.continue_);
        assert_eq!(r.flags.profile.as_deref(), Some("work"));
        assert_eq!(r.flags.effort.as_deref(), Some("low"));
        assert_eq!(r.passthru, os_args(&["extra"]));
    }

    #[test]
    fn value_flag_followed_by_dash_flag_does_not_consume_it() {
        // `--model` followed by `--effort` — `--effort` should NOT be consumed as
        // the model value (it starts with `-`), and should instead be parsed normally.
        let r = parse(&os_args(&["--model", "--effort", "high"]));
        // --model gets no value (None — next token starts with -)
        assert!(r.flags.model.is_none());
        // --effort is parsed correctly
        assert_eq!(r.flags.effort.as_deref(), Some("high"));
    }

    #[test]
    fn resume_followed_by_passthru_prompt() {
        // `csm --resume <uuid> "my follow-up"` — uuid is the resume id; the
        // prompt positional falls through to passthru.
        let r = parse(&os_args(&[
            "--resume",
            "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb",
            "my follow-up",
        ]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb".to_owned()
            ))
        );
        assert_eq!(r.passthru, os_args(&["my follow-up"]));
    }

    #[test]
    fn picker_promotion_then_other_flags_and_passthru() {
        // `-r` (no id) promotes to Picker; subsequent flags still parsed;
        // prompt positional falls to passthru.
        let r = parse(&os_args(&["-r", "--permission-mode=plan", "do the thing"]));
        assert_eq!(r.flags.resume, Some(ResumeArg::Picker));
        assert_eq!(r.flags.permission_mode.as_deref(), Some("plan"));
        assert_eq!(r.passthru, os_args(&["do the thing"]));
    }

    #[test]
    fn resume_equals_form_does_not_need_next_token() {
        // `--resume=<id>` is self-contained — no peeking at the next token
        let r = parse(&os_args(&[
            "--resume=aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb",
            "-c",
        ]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb".to_owned()
            ))
        );
        // `-c` is still consumed correctly after the equals-form resume
        assert!(r.flags.continue_);
    }

    #[test]
    fn profile_work_equals_form() {
        let r = parse(&os_args(&["--profile=work"]));
        assert_eq!(r.flags.profile.as_deref(), Some("work"));
    }

    #[test]
    fn profile_space_form_work() {
        let r = parse(&os_args(&["--profile", "work"]));
        assert_eq!(r.flags.profile.as_deref(), Some("work"));
    }

    #[test]
    fn unknown_flag_before_and_after_csm_flag() {
        // Interleaved: unknown flag, then csm flag, then unknown flag
        let r = parse(&os_args(&["--output-format=json", "--continue", "--print"]));
        assert!(r.flags.continue_);
        assert_eq!(r.passthru, os_args(&["--output-format=json", "--print"]));
    }

    /// The auto-resume handoff prompt `"resume"` is a plain positional arg.
    /// It must appear in passthru, not be silently swallowed.
    #[test]
    fn auto_handoff_prompt_falls_to_passthru() {
        let r = parse(&os_args(&[
            "--resume",
            "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb",
            "resume",
        ]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb".to_owned()
            ))
        );
        assert_eq!(r.passthru, os_args(&["resume"]));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // -h / --help: run's own help vs claude's
    //
    // `csm run --help` used to forward `--help` to claude, so run's flags were
    // undiscoverable from the subcommand itself (issue #25, "Related").
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn help_long_is_intercepted_when_nothing_forwarded_yet() {
        let r = parse(&os_args(&["--help"]));
        assert!(r.flags.help);
        assert!(r.passthru.is_empty(), "--help must not reach claude here");
    }

    #[test]
    fn help_short_is_intercepted_when_nothing_forwarded_yet() {
        let r = parse(&os_args(&["-h"]));
        assert!(r.flags.help);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn help_after_a_csm_flag_is_still_runs_help() {
        // csm's own flags do not count as "forwarded" — nothing is in passthru
        // yet, so this is still a question about `csm run`.
        let r = parse(&os_args(&["-c", "--help"]));
        assert!(r.flags.help);
        assert!(r.flags.continue_);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn help_after_a_passthru_token_forwards_to_claude() {
        // `-p` is claude's print flag: it lands in passthru first, so the
        // `--help` after it is a question about CLAUDE and must forward.
        let r = parse(&os_args(&["-p", "--help"]));
        assert!(!r.flags.help);
        assert_eq!(r.passthru, os_args(&["-p", "--help"]));
    }

    #[test]
    fn help_after_double_dash_forwards_to_claude() {
        let r = parse(&os_args(&["--", "--help"]));
        assert!(!r.flags.help);
        assert_eq!(r.passthru, os_args(&["--help"]));
    }

    #[test]
    fn help_absent_is_false() {
        let r = parse(&os_args(&["-c"]));
        assert!(!r.flags.help);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // ResumeArg display / discriminant checks
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn resume_arg_id_carries_value() {
        let r = parse(&os_args(&["-r", "some-alias"]));
        match r.flags.resume.unwrap() {
            ResumeArg::Id(s) => assert_eq!(s, "some-alias"),
            ResumeArg::Picker => panic!("expected Id, got Picker"),
        }
    }

    #[test]
    fn resume_arg_picker_discriminant() {
        let r = parse(&os_args(&["-r"]));
        assert!(matches!(r.flags.resume, Some(ResumeArg::Picker)));
    }
}
