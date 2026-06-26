//! Hand-rolled positional flag parser for `csm run`.
//!
//! **Why not `clap`?** Claude's own flags (`-c`, `-r`, `--model`, `--effort`,
//! …) must be forwarded verbatim to `claude`. A standard clap binding would
//! swallow them. This parser consumes only the flags that `csm` itself cares
//! about; everything else accumulates in `passthru`.
//!
//! Implements spec §2 "Arg parsing" in full:
//! - Consumed-internally flags: `-i`/`--interactive`, `-n`/`--new`,
//!   `-c`/`--continue`, `-A`/`--pick-account`, `--no-pick`, `-r`/`--resume`,
//!   `--permission-mode`, `--effort`, `--model`, `--session-id`, `--profile`.
//! - **Equals-form (N7):** `--resume=<id>`, `--permission-mode=<m>`,
//!   `--effort=<e>`, `--model=<m>`, `--session-id=<id>`, `--profile=<p>`.
//! - **`-r`/`--resume` alias resolution (N6):** non-UUID value → alias token;
//!   missing / dash-prefixed next token → promote to picker ([`ResumeArg::Picker`]).
//! - `--` stops parsing; everything after goes verbatim into `passthru`.
//! - Unknown flags / positional args go into `passthru`.
//!
//! The zsh source being reproduced is `claude-smart.zsh` lines 121–150
//! (the `while (( $# )); do … done` arg-parse block).

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
/// Mirrors the zsh logic at `claude-smart.zsh` lines 128–135:
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
/// Reproduces the local variables at `claude-smart.zsh` lines 116–118:
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
    /// `-n` / `--new` — start a fresh session (skip auto-resume).
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
/// Reproduces the `while (( $# )); do case "$1" in … esac; done` loop at
/// `claude-smart.zsh` lines 121–150 exactly, including:
/// - `--` passthrough terminator (line 147)
/// - `-r`/`--resume` picker promotion (lines 128–135)
/// - equals-form via zsh's `${1#--flag=}` (lines 138–146; Rust: `strip_prefix`)
/// - unrecognised args → `passthru` (line 148)
pub fn parse(args: &[OsString]) -> ParsedArgs {
    let mut flags = Flags::default();
    let mut passthru: Vec<OsString> = Vec::new();

    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();

        // `--` stops csm-side parsing; everything after is passthru.
        // Reproduces zsh line 147: `--)  shift; passthru+=("$@"); break ;;`
        if s == "--" {
            passthru.extend(iter.cloned());
            break;
        }

        // ── no-value boolean flags ─────────────────────────────────────────────
        // zsh lines 123–127
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
        // zsh lines 128–135:
        //   if [[ -n "${2:-}" && "$2" != -* ]]; then
        //     resume_id="$2"; shift 2
        //   else
        //     want_picker=true; shift
        //   fi
        if s == "-r" || s == "--resume" {
            flags.resume = Some(consume_value_or_picker(&mut iter));
            continue;
        }
        // zsh line 136: --resume=*)  resume_id="${1#--resume=}"; shift ;;
        if let Some(val) = strip_eq_prefix(&s, "--resume") {
            flags.resume = Some(ResumeArg::Id(val.to_owned()));
            continue;
        }

        // ── --permission-mode ──────────────────────────────────────────────────
        // zsh lines 137–138
        if s == "--permission-mode" {
            flags.permission_mode = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--permission-mode") {
            flags.permission_mode = Some(val.to_owned());
            continue;
        }

        // ── --effort ──────────────────────────────────────────────────────────
        // zsh lines 139–140
        if s == "--effort" {
            flags.effort = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--effort") {
            flags.effort = Some(val.to_owned());
            continue;
        }

        // ── --model ───────────────────────────────────────────────────────────
        // zsh lines 141–142
        if s == "--model" {
            flags.model = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--model") {
            flags.model = Some(val.to_owned());
            continue;
        }

        // ── --session-id ──────────────────────────────────────────────────────
        // zsh lines 143–144
        if s == "--session-id" {
            flags.session_id = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--session-id") {
            flags.session_id = Some(val.to_owned());
            continue;
        }

        // ── --profile ─────────────────────────────────────────────────────────
        // zsh lines 145–146
        if s == "--profile" {
            flags.profile = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--profile") {
            flags.profile = Some(val.to_owned());
            continue;
        }

        // Unrecognised argument: forward verbatim to claude.
        // zsh line 148: *)  passthru+=("$1"); shift ;;
        passthru.push(arg.clone());
    }

    ParsedArgs { flags, passthru }
}

// ─── internal helpers ─────────────────────────────────────────────────────────

/// Try to strip a `--flag=` prefix from `s`, returning the value slice.
/// Returns `None` if `s` does not start with `"{flag}="`.
///
/// Reproduces zsh's `${1#--flag=}` strip form (lines 136, 138, 140, 142, 144, 146).
fn strip_eq_prefix<'a>(s: &'a str, flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    s.strip_prefix(prefix.as_str())
}

/// Peek at the next argument. If it exists and does NOT start with `-`, consume
/// it and return `ResumeArg::Id(value)`. Otherwise return `ResumeArg::Picker`
/// (promote to picker intent) without advancing the iterator.
///
/// Reproduces zsh's `-r`/`--resume` guard at lines 131–134:
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
        assert!(!r.flags.new);
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
    // ResumeArg — equals-form (N7)
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
    // Reproduces zsh lines 128–135: when the next token is absent OR starts
    // with '-', want_picker=true (our ResumeArg::Picker).
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
        let r = parse(&os_args(&["-n"]));
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
    // Value flags — equals-form (N7)
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
    // Space-form vs equals-form equivalence (N7 parity)
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
    //
    // Reproduces zsh line 147: `--)  shift; passthru+=("$@"); break ;;`
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn double_dash_stops_parsing_passes_rest() {
        let r = parse(&os_args(&["-n", "--", "--model", "raw-arg"]));
        assert!(r.flags.new);
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
        let r = parse(&os_args(&["--", "-n", "--model", "x"]));
        assert!(!r.flags.new);
        assert_eq!(r.passthru, os_args(&["-n", "--model", "x"]));
    }

    #[test]
    fn double_dash_preserves_multiple_args() {
        let r = parse(&os_args(&["--", "foo", "bar", "baz"]));
        assert_eq!(r.passthru, os_args(&["foo", "bar", "baz"]));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Passthru: claude's own flags fall through unchanged
    //
    // Reproduces zsh line 148: *)  passthru+=("$1"); shift ;;
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
            "-n",
            "my prompt",
        ]));
        assert!(r.flags.new);
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
        let r = parse(&os_args(&["-i", "-n", "-c", "-A", "--no-pick"]));
        assert!(r.flags.interactive);
        assert!(r.flags.new);
        assert!(r.flags.continue_);
        assert!(r.flags.pick_account);
        assert!(r.flags.no_pick);
        assert!(r.passthru.is_empty());
    }

    #[test]
    fn all_boolean_flags_long_forms() {
        let r = parse(&os_args(&[
            "--interactive",
            "--new",
            "--continue",
            "--pick-account",
            "--no-pick",
        ]));
        assert!(r.flags.interactive);
        assert!(r.flags.new);
        assert!(r.flags.continue_);
        assert!(r.flags.pick_account);
        assert!(r.flags.no_pick);
    }

    /// Reproduces the real-world `csm -i -A` invocation from the zsh source
    /// doc-comment (line 71: "pick a session AND the best account").
    #[test]
    fn interactive_plus_pick_account() {
        let r = parse(&os_args(&["-i", "-A"]));
        assert!(r.flags.interactive);
        assert!(r.flags.pick_account);
        assert!(!r.flags.no_pick);
        assert!(r.passthru.is_empty());
    }

    /// `csm --permission-mode plan --effort high` from zsh doc (line 66)
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
            "-n",
            "--profile=work",
            "--effort",
            "low",
            "--",
            "extra",
        ]));
        assert!(r.flags.new);
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
            "-n",
        ]));
        assert_eq!(
            r.flags.resume,
            Some(ResumeArg::Id(
                "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb".to_owned()
            ))
        );
        // `-n` is still consumed correctly after the equals-form resume
        assert!(r.flags.new);
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
        let r = parse(&os_args(&["--output-format=json", "--new", "--print"]));
        assert!(r.flags.new);
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
