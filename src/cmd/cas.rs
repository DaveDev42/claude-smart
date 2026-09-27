//! `csm cas` — deprecated compat for the retired profile switcher.
//!
//! csm keeps no profile registry any more (Orca owns the accounts), so the
//! eval-class shim contract is gone. Two spellings survive so old shell
//! shims and launchd/HKCU floors degrade quietly instead of breaking a login
//! shell:
//!
//! - `csm cas --print-default-dir` prints Orca's live `D` while that is a
//!   legacy profile dir (and, before the migration's cutover with Orca not
//!   running, csm's own `D` when that is one), else nothing and a stderr
//!   note: exporting `~/.claude` would make claude read
//!   `~/.claude/.claude.json` instead of `~/.claude.json`.
//! - `csm cas --eval …` prints NOTHING on stdout (so `eval "$(…)"` is a
//!   no-op), one deprecation line on stderr, and exits 0.
//!
//! Every other form (the old management verbs) fails with a pointer to
//! `csm accounts`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::launch_context::OrcaMain;

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

/// The stderr note when `--print-default-dir` prints nothing.
pub(crate) const NOTHING_TO_PRINT: &str = "csm: `csm cas --print-default-dir` prints nothing: claude runs in \
     ~/.claude without CLAUDE_CONFIG_DIR now (remove the shell line that exports it)";

/// What `--print-default-dir` prints (design section 5), `None` for
/// nothing. `orca`: Orca main's `D`; `own_d`: csm's `D`; `cutover`: the
/// migration recorded its cutover; `legacy`: the recorded legacy profile
/// dirs (never `~/.claude`). Orca's live `D` is printed while it is a
/// legacy dir; with Orca stopped or unreadable, csm's own `D` is, but only
/// before the cutover (the floor still points shells there). Pure.
pub(crate) fn print_default_dir(
    orca: &OrcaMain,
    own_d: &Path,
    cutover: bool,
    legacy: &[PathBuf],
) -> Option<PathBuf> {
    let trim = |p: &Path| PathBuf::from(p.to_string_lossy().trim_end_matches(['/', '\\']));
    let is_legacy = |p: &Path| legacy.iter().any(|l| trim(l) == trim(p));
    match orca {
        OrcaMain::Dir(o) => is_legacy(&o.dir).then(|| o.dir.clone()),
        OrcaMain::Stopped | OrcaMain::Unreadable => {
            (!cutover && is_legacy(own_d)).then(|| own_d.to_path_buf())
        }
    }
}

/// `csm cas …` (deprecated).
pub(crate) fn cmd_cas(args: &[OsString]) -> anyhow::Result<()> {
    match classify(args) {
        CasCompat::PrintDefaultDir => {
            let env = crate::orca::HostEnv::current()?;
            let own_d = crate::orca::runtime::runtime_paths(
                env.claude_config_dir.as_deref(),
                &env.home,
                |p| p.exists(),
            )
            .config_dir;
            let (mut legacy, cutover, _) = crate::migrate::stale_dirs(&env);
            let home_d = env.home.join(".claude");
            legacy.retain(|d| *d != home_d);
            let orca = crate::launch_context::orca_main_dir(&env);
            match print_default_dir(&orca, &own_d, cutover, &legacy) {
                Some(d) => print_default_dir_to(&mut std::io::stdout(), &d)?,
                None => eprintln!("{NOTHING_TO_PRINT}"),
            }
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

    #[test]
    fn print_default_dir_follows_orca_while_it_runs_in_a_legacy_dir() {
        use crate::orca::procenv::OrcaDir;
        let home = Path::new("/Users/example");
        let work = home.join(".claude.work");
        let d = home.join(".claude");
        let legacy = vec![work.clone(), home.join(".claude.home")];
        let orca = |dir: &Path| {
            OrcaMain::Dir(OrcaDir {
                dir: dir.to_path_buf(),
                explicit: true,
            })
        };
        // Orca runs in a legacy dir: that dir, before or after the cutover.
        for cutover in [false, true] {
            assert_eq!(
                print_default_dir(&orca(&work), &d, cutover, &legacy),
                Some(work.clone())
            );
        }
        // Orca runs in ~/.claude: nothing (exporting it would break I3).
        assert_eq!(print_default_dir(&orca(&d), &work, false, &legacy), None);
        // Orca stopped: csm's own legacy D before the cutover, never after.
        assert_eq!(
            print_default_dir(&OrcaMain::Stopped, &work, false, &legacy),
            Some(work.clone())
        );
        assert_eq!(
            print_default_dir(&OrcaMain::Stopped, &work, true, &legacy),
            None
        );
        assert_eq!(
            print_default_dir(&OrcaMain::Unreadable, &d, false, &legacy),
            None
        );
        assert!(!NOTHING_TO_PRINT.contains('\n'));
    }
}
