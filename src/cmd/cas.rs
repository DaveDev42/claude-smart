//! `csm cas` — the eval-class shim contract (machine interface).
//!
//! Parses `--eval`/`--shell`/`--print-default-dir` flags and the CAS
//! operation, then dispatches to `cas::eval_emit` (profile switching, eval-able
//! output) or `cas::manage_emit` (registry management verbs, human output).

use std::ffi::OsString;

use anyhow::Context as _;

use crate::{account, cas};

/// `csm cas --eval --shell {zsh|pwsh} -- <op args...>`
///
/// Parses flags and the CAS operation, loads `profiles.json`, and calls
/// `cas::eval_emit` which emits the eval-able export line (or shell error
/// snippet) to stdout.
///
/// Called from the shell shim as:
///   `eval "$(command csm cas --eval --shell zsh -- "$@")"`
///   `Invoke-Expression (csm cas --eval --shell pwsh -- @args)`
pub(crate) fn cmd_cas(args: &[OsString]) -> anyhow::Result<()> {
    use cas::{Op, Shell};

    let parsed = parse_cas_flags(args)?;

    // `--print-default-dir`: print the resolved default CLAUDE_CONFIG_DIR and
    // return. Used by the shell/launchd floors as the single SSOT for dir
    // derivation (no `--shell`, no eval). Takes precedence over op parsing.
    if parsed.print_default_dir {
        let profiles = account::ProfileMap::load().unwrap_or_default();
        print_default_dir_to(&mut std::io::stdout(), &profiles)?;
        return Ok(());
    }

    let eval_mode = parsed.eval_mode;
    let op = parse_cas_op(&parsed.op_args)?;

    // Registry-management ops are non-eval (the shim calls `csm cas <verb>`
    // directly). They mutate profiles.json / the default state file and print
    // human output — not an eval-able line.
    if matches!(
        op,
        Op::List
            | Op::Add { .. }
            | Op::Set { .. }
            | Op::Remove { .. }
            | Op::SetDefault { .. }
            | Op::Edit
    ) {
        if eval_mode {
            anyhow::bail!("csm cas: management verbs ({op:?}) must not be wrapped in --eval");
        }
        let mut profiles =
            account::ProfileMap::load().context("csm cas: failed to load profiles.json")?;
        return cas::manage_emit(&op, &mut profiles);
    }

    let shell = if eval_mode {
        let s = parsed
            .shell
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("csm cas: --eval requires --shell <zsh|pwsh>"))?;
        Shell::parse(s).ok_or_else(|| anyhow::anyhow!("csm cas: unknown --shell value {s:?}"))?
    } else {
        Shell::Zsh // informational status path
    };

    if !eval_mode {
        // Without --eval only Op::Status is allowed among the eval-class ops.
        if !matches!(op, Op::Status { .. }) {
            anyhow::bail!("csm cas: --eval flag is required for profile switching");
        }
    }

    let profiles = account::ProfileMap::load().context("csm cas: failed to load profiles.json")?;
    cas::eval_emit(shell, &op, &profiles)
}

/// Thin shell over `--print-default-dir`'s output: print the resolved default
/// `CLAUDE_CONFIG_DIR` to `w`. Pure core so the golden tests can assert the
/// exact bytes without going through real stdout.
pub(crate) fn print_default_dir_to(
    w: &mut impl std::io::Write,
    profiles: &account::ProfileMap,
) -> std::io::Result<()> {
    writeln!(w, "{}", profiles.default_dir().to_string_lossy())
}

/// Parsed `csm cas` flags.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CasFlags {
    pub(crate) eval_mode: bool,
    pub(crate) shell: Option<String>,
    pub(crate) op_args: Vec<String>,
    /// `--print-default-dir`: print the resolved default dir and exit (floor SSOT).
    pub(crate) print_default_dir: bool,
}

/// Parse `--eval`, `--shell`, `--print-default-dir`, and `--` sections from
/// `csm cas` arguments.
pub(crate) fn parse_cas_flags(args: &[OsString]) -> anyhow::Result<CasFlags> {
    let mut f = CasFlags::default();
    let mut past_double_dash = false;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if past_double_dash {
            f.op_args.push(arg.to_string_lossy().into_owned());
            continue;
        }
        let s = arg.to_string_lossy();
        if s == "--" {
            past_double_dash = true;
        } else if s == "--eval" {
            f.eval_mode = true;
        } else if s == "--print-default-dir" {
            f.print_default_dir = true;
        } else if s == "--shell" {
            if let Some(next) = iter.next() {
                f.shell = Some(next.to_string_lossy().into_owned());
            }
        } else if let Some(val) = s.strip_prefix("--shell=") {
            f.shell = Some(val.to_owned());
        } else {
            // Positional arg before `--`: treat as start of op args.
            f.op_args.push(s.into_owned());
            for remaining in iter.by_ref() {
                f.op_args.push(remaining.to_string_lossy().into_owned());
            }
            break;
        }
    }

    Ok(f)
}

/// Parse the CAS operation from the op-args slice.
pub(crate) fn parse_cas_op(op_args: &[String]) -> anyhow::Result<cas::Op> {
    use cas::Op;
    match op_args.first().map(String::as_str) {
        None | Some("status") => {
            let print_current = op_args
                .get(1)
                .map(|s| s == "--print-current")
                .unwrap_or(false);
            Ok(Op::Status { print_current })
        }
        Some("-") => Ok(Op::Minus),
        Some("resync") => Ok(Op::Resync),
        Some("-g") | Some("--global") => {
            let profile = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("csm cas: -g/--global requires a profile argument")
            })?;
            Ok(Op::Global { profile })
        }
        // ── registry management verbs (reserved words; routed to manage_emit) ──
        Some("list") => Ok(Op::List),
        Some("add") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("add: requires a profile name (`csm profiles add <name> [<dir>]`)")
            })?;
            Ok(Op::Add {
                name,
                dir: op_args.get(2).cloned(),
            })
        }
        Some("set") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("set: requires <name> <dir> (`csm profiles set <name> <dir>`)")
            })?;
            let dir = op_args.get(2).cloned().ok_or_else(|| {
                anyhow::anyhow!("set: requires <name> <dir> (`csm profiles set <name> <dir>`)")
            })?;
            Ok(Op::Set { name, dir })
        }
        Some("remove") | Some("rm") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("remove: requires a profile name (`csm profiles rm <name>`)")
            })?;
            Ok(Op::Remove { name })
        }
        Some("use") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("use: requires a profile name (`csm profiles use <name>`)")
            })?;
            Ok(Op::SetDefault { name })
        }
        Some("edit") => Ok(Op::Edit),
        Some(profile) => Ok(Op::Switch {
            profile: profile.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
    }

    // ── parse_cas_flags ───────────────────────────────────────────────────────

    #[test]
    fn parse_cas_flags_eval_shell_double_dash() {
        let args = argv(&["--eval", "--shell", "zsh", "--", "home"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.eval_mode);
        assert_eq!(f.shell.as_deref(), Some("zsh"));
        assert_eq!(f.op_args, vec!["home"]);
        assert!(!f.print_default_dir);
    }

    #[test]
    fn parse_cas_flags_equals_form_shell() {
        let args = argv(&["--eval", "--shell=pwsh", "--", "work"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.eval_mode);
        assert_eq!(f.shell.as_deref(), Some("pwsh"));
        assert_eq!(f.op_args, vec!["work"]);
    }

    #[test]
    fn parse_cas_flags_no_eval_mode() {
        let args = argv(&["status"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(!f.eval_mode);
        assert_eq!(f.op_args, vec!["status"]);
    }

    #[test]
    fn parse_cas_flags_global_op() {
        let args = argv(&["--eval", "--shell", "zsh", "--", "-g", "home"]);
        let f = parse_cas_flags(&args).unwrap();
        assert_eq!(f.op_args, vec!["-g", "home"]);
    }

    #[test]
    fn parse_cas_flags_print_default_dir() {
        let args = argv(&["--print-default-dir"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.print_default_dir);
        assert!(!f.eval_mode);
        assert!(f.op_args.is_empty());
    }

    // ── parse_cas_op ──────────────────────────────────────────────────────────

    #[test]
    fn parse_cas_op_switch() {
        let op = parse_cas_op(&["home".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Switch { profile } if profile == "home"));
    }

    #[test]
    fn parse_cas_op_minus() {
        let op = parse_cas_op(&["-".to_owned()]).unwrap();
        assert_eq!(op, cas::Op::Minus);
    }

    #[test]
    fn parse_cas_op_global() {
        let op = parse_cas_op(&["-g".to_owned(), "work".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Global { profile } if profile == "work"));
    }

    #[test]
    fn parse_cas_op_resync() {
        let op = parse_cas_op(&["resync".to_owned()]).unwrap();
        assert_eq!(op, cas::Op::Resync);
    }

    #[test]
    fn parse_cas_op_status_no_args() {
        let op = parse_cas_op(&[]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: false
            }
        ));
    }

    #[test]
    fn parse_cas_op_status_explicit() {
        let op = parse_cas_op(&["status".to_owned()]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: false
            }
        ));
    }

    #[test]
    fn parse_cas_op_status_print_current() {
        let op = parse_cas_op(&["status".to_owned(), "--print-current".to_owned()]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: true
            }
        ));
    }

    #[test]
    fn parse_cas_op_global_long_form() {
        let op = parse_cas_op(&["--global".to_owned(), "home".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Global { profile } if profile == "home"));
    }

    // ── parse_cas_op: registry management verbs ───────────────────────────────

    #[test]
    fn parse_cas_op_list() {
        assert!(matches!(
            parse_cas_op(&["list".to_owned()]).unwrap(),
            cas::Op::List
        ));
    }

    #[test]
    fn parse_cas_op_add_with_and_without_dir() {
        let op = parse_cas_op(&["add".to_owned(), "work".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Add { ref name, dir: None } if name == "work"));
        let op = parse_cas_op(&["add".to_owned(), "work".to_owned(), "/d".to_owned()]).unwrap();
        assert!(
            matches!(op, cas::Op::Add { ref name, dir: Some(ref d) } if name == "work" && d == "/d")
        );
        // missing name → err
        assert!(parse_cas_op(&["add".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_set_requires_name_and_dir() {
        let op = parse_cas_op(&["set".to_owned(), "w".to_owned(), "/d".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Set { ref name, ref dir } if name == "w" && dir == "/d"));
        assert!(parse_cas_op(&["set".to_owned(), "w".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_remove_and_rm_alias() {
        let op = parse_cas_op(&["remove".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Remove { ref name } if name == "w"));
        let op = parse_cas_op(&["rm".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Remove { ref name } if name == "w"));
        assert!(parse_cas_op(&["remove".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_use_sets_default() {
        let op = parse_cas_op(&["use".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::SetDefault { ref name } if name == "w"));
        assert!(parse_cas_op(&["use".to_owned()]).is_err());
    }

    // ── print_default_dir_to: golden bytes (test-05) ──────────────────────────

    /// Run `f` with `HOME` pointed at a fresh temp dir, so
    /// `ProfileMap::default_dir()`'s state-file read never touches the
    /// developer's real `~/.config/claude-as/default`. Module-local lock,
    /// mirroring the pattern documented in `crate::testenv`.
    fn with_isolated_home<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = HOME_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let result = f(tmp.path());
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    #[test]
    fn golden_print_default_dir_known_profile() {
        with_isolated_home(|home| {
            let dir = home.join(".claude.home").to_string_lossy().into_owned();
            let mut m = std::collections::HashMap::new();
            m.insert("home".to_owned(), dir.clone());
            m.insert(
                "work".to_owned(),
                home.join(".claude.work").to_string_lossy().into_owned(),
            );
            let profiles = account::ProfileMap(m);
            let mut buf = Vec::new();
            // Isolated HOME has no state file, so the default is the
            // alphabetical-first profile — "home".
            print_default_dir_to(&mut buf, &profiles).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), format!("{dir}\n"));
        });
    }

    #[test]
    fn golden_print_default_dir_empty_registry_synthesizes() {
        with_isolated_home(|home| {
            let profiles = account::ProfileMap::default();
            let mut buf = Vec::new();
            print_default_dir_to(&mut buf, &profiles).unwrap();
            let expected = home.join(".claude.").to_string_lossy().into_owned();
            let out = String::from_utf8(buf).unwrap();
            assert!(
                out.starts_with(&expected) && out.ends_with('\n'),
                "got: {out}"
            );
        });
    }
}
