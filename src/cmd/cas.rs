//! `csm cas` — deprecated compat for the retired profile switcher.
//!
//! csm keeps no profile registry any more (Orca owns the accounts), so the
//! eval-class shim contract is gone. Two spellings survive so old shell
//! shims and launchd/HKCU floors degrade quietly instead of breaking a login
//! shell:
//!
//! - `csm cas --print-default-dir` prints csm's runtime dir `D` (Orca's
//!   `CLAUDE_CONFIG_DIR`, else `~/.claude`).
//! - `csm cas --eval …` prints NOTHING on stdout (so `eval "$(…)"` is a
//!   no-op), one deprecation line on stderr, and exits 0.
//!
//! Every other form (the old management verbs) fails with a pointer to
//! `csm accounts`.

use std::ffi::OsString;
use std::path::Path;

/// What a `csm cas` invocation maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CasCompat {
    /// `--print-default-dir`: print `D`.
    PrintDefaultDir,
    /// `--eval …`: stdout stays empty.
    EvalNoop,
    /// Anything else: retired.
    Retired,
}

/// The stderr line for the no-op `--eval`.
pub(crate) const EVAL_DEPRECATION: &str =
    "csm: `csm cas` is deprecated and does nothing (Orca owns accounts now; see `csm accounts`)";

/// Pure core: classify the args. `--print-default-dir` wins over `--eval`,
/// as it did before; only tokens before `--` are read.
pub(crate) fn classify(args: &[OsString]) -> CasCompat {
    let flags: Vec<_> = args
        .iter()
        .map(|a| a.to_string_lossy())
        .take_while(|a| a != "--")
        .collect();
    if flags.iter().any(|a| a == "--print-default-dir") {
        CasCompat::PrintDefaultDir
    } else if flags.iter().any(|a| a == "--eval") {
        CasCompat::EvalNoop
    } else {
        CasCompat::Retired
    }
}

/// Write `D` and a newline to `w`.
pub(crate) fn print_default_dir_to(w: &mut impl std::io::Write, d: &Path) -> std::io::Result<()> {
    writeln!(w, "{}", d.to_string_lossy())
}

/// `csm cas …` (deprecated).
pub(crate) fn cmd_cas(args: &[OsString]) -> anyhow::Result<()> {
    match classify(args) {
        CasCompat::PrintDefaultDir => {
            let env = crate::orca::HostEnv::current()?;
            let d = crate::orca::runtime::runtime_paths(
                env.claude_config_dir.as_deref(),
                &env.home,
                |p| p.exists(),
            )
            .config_dir;
            print_default_dir_to(&mut std::io::stdout(), &d)?;
            Ok(())
        }
        CasCompat::EvalNoop => {
            eprintln!("{EVAL_DEPRECATION}");
            Ok(())
        }
        CasCompat::Retired => anyhow::bail!(
            "csm cas: the profile switcher was removed; manage accounts with `csm accounts`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    #[test]
    fn print_default_dir_is_recognised() {
        assert_eq!(
            classify(&os(&["--print-default-dir"])),
            CasCompat::PrintDefaultDir
        );
        assert_eq!(
            classify(&os(&["--eval", "--print-default-dir"])),
            CasCompat::PrintDefaultDir
        );
    }

    #[test]
    fn eval_is_a_quiet_noop() {
        assert_eq!(
            classify(&os(&["--eval", "--shell", "zsh", "--", "work"])),
            CasCompat::EvalNoop
        );
        assert!(!EVAL_DEPRECATION.contains('\n'));
    }

    #[test]
    fn flags_after_double_dash_are_ignored() {
        assert_eq!(classify(&os(&["--", "--eval"])), CasCompat::Retired);
    }

    #[test]
    fn management_verbs_are_retired() {
        assert_eq!(classify(&os(&["list"])), CasCompat::Retired);
        assert_eq!(classify(&[]), CasCompat::Retired);
    }

    #[test]
    fn print_default_dir_writes_exactly_d() {
        let mut buf = Vec::new();
        print_default_dir_to(&mut buf, Path::new("/Users/example/.claude")).unwrap();
        assert_eq!(buf, b"/Users/example/.claude\n");
    }
}
