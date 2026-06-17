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
//!   missing / dash-prefixed next token → promote to picker (None).
//! - `--` stops parsing; everything after goes verbatim into `passthru`.
//! - Unknown flags / positional args go into `passthru`.

use std::ffi::OsString;

/// Flags consumed and interpreted by `csm run` itself.
///
/// `None` means the flag was absent; `Some(None)` for Option-wrapped pickers
/// means "open picker" (the flag was present but no value followed).
#[derive(Debug, Default, PartialEq)]
pub struct Flags {
    /// `-i` / `--interactive` — force interactive (TTY) mode.
    pub interactive: bool,
    /// `-n` / `--new` — start a fresh session (skip auto-resume).
    pub new: bool,
    /// `-c` / `--continue` — continue the newest free session.
    pub r#continue: bool,
    /// `-A` / `--pick-account` — force an account pick.
    pub pick_account: bool,
    /// `--no-pick` — suppress all automatic account picking.
    pub no_pick: bool,
    /// `-r` / `--resume [<id-or-alias>]`
    ///   - `None`               → flag absent
    ///   - `Some(None)`         → flag present, open picker
    ///   - `Some(Some(s))`      → value supplied (UUID or alias token)
    pub resume: Option<Option<String>>,
    /// `--permission-mode <m>` / `--permission-mode=<m>`
    pub permission_mode: Option<String>,
    /// `--effort <e>` / `--effort=<e>`
    pub effort: Option<String>,
    /// `--model <m>` / `--model=<m>`
    pub model: Option<String>,
    /// `--session-id <id>` / `--session-id=<id>`
    pub session_id: Option<String>,
    /// `--profile <p>` / `--profile=<p>`
    pub profile: Option<String>,
}

/// Result of parsing `csm run` arguments.
#[derive(Debug, Default)]
pub struct ParsedArgs {
    /// Flags consumed internally by `csm run`.
    pub flags: Flags,
    /// Everything not consumed — forwarded verbatim to `claude`.
    pub passthru: Vec<OsString>,
}

/// Parse the arguments for `csm run` from a slice of OS strings.
///
/// The slice should be the arguments **after** the subcommand word has been
/// consumed by `main.rs`'s dispatcher (i.e. `args[2..]` or `args[1..]`
/// depending on dispatch mode).
///
/// # Phase-0 note
///
/// The body is real and fully implemented; the spec designates `cli/parser.rs`
/// as REAL (pure, no OS coupling, table-driven unit tests required).
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

        // ── short flags (no value) ──────────────────────────────────────────
        if s == "-i" || s == "--interactive" {
            flags.interactive = true;
            continue;
        }
        if s == "-n" || s == "--new" {
            flags.new = true;
            continue;
        }
        if s == "-c" || s == "--continue" {
            flags.r#continue = true;
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

        // ── -r / --resume [<id-or-alias>] ──────────────────────────────────
        if s == "-r" || s == "--resume" {
            flags.resume = Some(consume_value_or_picker(&mut iter));
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--resume") {
            flags.resume = Some(Some(val.to_owned()));
            continue;
        }

        // ── --permission-mode ───────────────────────────────────────────────
        if s == "--permission-mode" {
            flags.permission_mode = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--permission-mode") {
            flags.permission_mode = Some(val.to_owned());
            continue;
        }

        // ── --effort ────────────────────────────────────────────────────────
        if s == "--effort" {
            flags.effort = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--effort") {
            flags.effort = Some(val.to_owned());
            continue;
        }

        // ── --model ─────────────────────────────────────────────────────────
        if s == "--model" {
            flags.model = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--model") {
            flags.model = Some(val.to_owned());
            continue;
        }

        // ── --session-id ────────────────────────────────────────────────────
        if s == "--session-id" {
            flags.session_id = consume_required_value(&mut iter);
            continue;
        }
        if let Some(val) = strip_eq_prefix(&s, "--session-id") {
            flags.session_id = Some(val.to_owned());
            continue;
        }

        // ── --profile ───────────────────────────────────────────────────────
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
fn strip_eq_prefix<'a>(s: &'a str, flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    s.strip_prefix(prefix.as_str())
}

/// Peek at the next argument. If it exists and does NOT start with `-`, consume
/// and return `Some(value)`. Otherwise return `None` (promote to picker / no
/// value).
///
/// Used for `-r`/`--resume` where a missing or dash-prefixed next token means
/// "open picker".
fn consume_value_or_picker(
    iter: &mut std::iter::Peekable<std::slice::Iter<'_, OsString>>,
) -> Option<String> {
    match iter.peek() {
        Some(next) if !next.to_string_lossy().starts_with('-') => {
            Some(iter.next().unwrap().to_string_lossy().into_owned())
        }
        _ => None, // promote to picker
    }
}

/// Consume the next argument as a required value for a named flag.
/// If there is no next argument (or the next starts with `-`), returns `None`
/// and does NOT advance the iterator (the next token may be a different flag).
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

/// Build a `Vec<OsString>` from string slices — test helper.
#[cfg(test)]
fn os_args(ss: &[&str]) -> Vec<OsString> {
    ss.iter().map(|s| OsString::from(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── basic boolean flags ───────────────────────────────────────────────────

    #[test]
    fn parse_interactive_short() {
        let r = parse(&os_args(&["-i"]));
        assert!(r.flags.interactive);
    }

    #[test]
    fn parse_new_short() {
        let r = parse(&os_args(&["-n"]));
        assert!(r.flags.new);
    }

    #[test]
    fn parse_continue_short() {
        let r = parse(&os_args(&["-c"]));
        assert!(r.flags.r#continue);
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

    // ── resume forms ─────────────────────────────────────────────────────────

    #[test]
    fn parse_resume_with_uuid() {
        let r = parse(&os_args(&["-r", "01234567-89ab-cdef-0123-456789abcdef"]));
        assert_eq!(
            r.flags.resume,
            Some(Some("01234567-89ab-cdef-0123-456789abcdef".to_owned()))
        );
    }

    #[test]
    fn parse_resume_with_alias() {
        let r = parse(&os_args(&["--resume", "my-session-alias"]));
        assert_eq!(
            r.flags.resume,
            Some(Some("my-session-alias".to_owned()))
        );
    }

    #[test]
    fn parse_resume_equals_form() {
        let r = parse(&os_args(&["--resume=abc-def"]));
        assert_eq!(r.flags.resume, Some(Some("abc-def".to_owned())));
    }

    #[test]
    fn parse_resume_missing_value_promotes_to_picker() {
        // No next token → picker
        let r = parse(&os_args(&["-r"]));
        assert_eq!(r.flags.resume, Some(None));
    }

    #[test]
    fn parse_resume_dash_prefixed_next_promotes_to_picker() {
        // Next token starts with '-' → promote to picker, leave it in the
        // iterator so it is parsed as its own flag
        let r = parse(&os_args(&["-r", "--model", "opus"]));
        assert_eq!(r.flags.resume, Some(None));
        assert_eq!(r.flags.model.as_deref(), Some("opus"));
    }

    // ── equals-form flags (N7) ────────────────────────────────────────────────

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
        let r = parse(&os_args(&["--session-id=aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb"]));
        assert_eq!(
            r.flags.session_id.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb")
        );
    }

    #[test]
    fn parse_profile_equals() {
        let r = parse(&os_args(&["--profile=personal"]));
        assert_eq!(r.flags.profile.as_deref(), Some("personal"));
    }

    // ── passthru / `--` separator ─────────────────────────────────────────────

    #[test]
    fn passthru_unknown_flag() {
        // Claude-own flag should pass through untouched
        let r = parse(&os_args(&["--dangerously-skip-permissions"]));
        assert_eq!(r.passthru, os_args(&["--dangerously-skip-permissions"]));
    }

    #[test]
    fn double_dash_stops_parsing() {
        let r = parse(&os_args(&["-n", "--", "--model", "raw-arg"]));
        assert!(r.flags.new);
        assert!(!r.flags.model.is_some()); // --model after -- is passthru
        assert_eq!(r.passthru, os_args(&["--model", "raw-arg"]));
    }

    #[test]
    fn empty_args() {
        let r = parse(&[]);
        assert_eq!(r.flags, Flags::default());
        assert!(r.passthru.is_empty());
    }

    // ── combined ─────────────────────────────────────────────────────────────

    #[test]
    fn combined_flags_and_passthru() {
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
    fn space_form_and_equals_form_equivalent_for_model() {
        let r_space = parse(&os_args(&["--model", "claude-sonnet-4-5"]));
        let r_eq = parse(&os_args(&["--model=claude-sonnet-4-5"]));
        assert_eq!(r_space.flags.model, r_eq.flags.model);
    }
}
