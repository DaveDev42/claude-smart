//! `csm migrate [--dry-run]`: run the automatic migration off the legacy
//! per-profile layout now and report where it stands.
//!
//! The work itself lives in [`crate::migrate`]; every FULL trigger (an
//! interactive launch, `csm orca setup`, the `accounts` verbs that change
//! something) runs it too, so this verb is for the fleet's converge and for
//! a look. `--dry-run` reports from the gates without writing. The former
//! `plan`, `import` and `retire` verbs are gone: they print a pointer and
//! exit 1.
//!
//! Exit status ([`exit_code`]): 0 when nothing legacy is left or the
//! cutover is recorded (the floor setters may go, even while retiring
//! waits for a reboot, and even while a later step reports an error: the
//! report still lists it), 75 while something is pending (retry at the
//! next converge), 1 on an error before the cutover.

use std::ffi::OsString;

use anyhow::bail;

use crate::migrate::{self, Report};

// ─── parser ───────────────────────────────────────────────────────────────────

/// `csm migrate`'s verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MigrateCmd {
    Run {
        dry_run: bool,
    },
    Help,
    /// `plan`, `import` or `retire`.
    Retired(String),
}

/// Parse the words after `migrate`. Pure.
pub(crate) fn parse(args: &[OsString]) -> anyhow::Result<MigrateCmd> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Ok(MigrateCmd::Run { dry_run: false }),
        ["--dry-run"] => Ok(MigrateCmd::Run { dry_run: true }),
        ["-h" | "--help" | "help"] => Ok(MigrateCmd::Help),
        [verb @ ("plan" | "import" | "retire"), ..] => Ok(MigrateCmd::Retired((*verb).to_owned())),
        _ => bail!("csm migrate: expected no argument or `--dry-run`"),
    }
}

/// The pointer a retired verb prints. Pure.
pub(crate) fn retired_line(verb: &str) -> String {
    format!(
        "csm migrate {verb}: this verb is gone; csm now migrates on its own. Run `csm migrate` \
         (or `csm migrate --dry-run` to look first)."
    )
}

// ─── exit status ──────────────────────────────────────────────────────────────

/// Exit `EX_TEMPFAIL`: pending, retry at the next converge.
pub(crate) const EXIT_PENDING: i32 = 75;

/// The exit status for a run's report. A recorded cutover (on disk) is 0
/// whatever else the run reports: every step before it had settled (I1),
/// so what fails later (a B1 name skipped, a B2 merge, a retire) never
/// needs the floor setters back, and a 1 would keep the fleet from
/// removing a setter that re-sets the floor at every login. Pure.
pub(crate) fn exit_code(r: &Report) -> i32 {
    if r.cutover_recorded && !r.busy {
        0
    } else if !r.errors.is_empty() {
        1
    } else if r.busy {
        EXIT_PENDING
    } else if !r.legacy {
        0
    } else {
        EXIT_PENDING
    }
}

// ─── command ──────────────────────────────────────────────────────────────────

pub(crate) fn cmd_migrate(args: &[OsString]) -> anyhow::Result<()> {
    match parse(args)? {
        MigrateCmd::Help => {
            println!("csm migrate [--dry-run]");
            println!("  move this machine off the legacy per-profile layout onto Orca's accounts");
            println!("  (csm also does this on its own at the next interactive launch)");
            println!("  --dry-run  report what would be done; write nothing");
            println!("exit: 0 done or cut over, 75 pending (run again later), 1 error");
            Ok(())
        }
        MigrateCmd::Retired(verb) => {
            eprintln!("{}", retired_line(&verb));
            std::process::exit(1);
        }
        MigrateCmd::Run { dry_run } => {
            let r = migrate::run(dry_run);
            print!("{}", migrate::render(&r));
            match exit_code(&r) {
                0 => Ok(()),
                code => {
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    std::process::exit(code);
                }
            }
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    #[test]
    fn parse_run_help_and_retired_verbs() {
        assert_eq!(parse(&[]).unwrap(), MigrateCmd::Run { dry_run: false });
        assert_eq!(
            parse(&os(&["--dry-run"])).unwrap(),
            MigrateCmd::Run { dry_run: true }
        );
        assert_eq!(parse(&os(&["--help"])).unwrap(), MigrateCmd::Help);
        for verb in ["plan", "import", "retire"] {
            assert_eq!(
                parse(&os(&[verb, "--dry-run"])).unwrap(),
                MigrateCmd::Retired(verb.into())
            );
            assert!(retired_line(verb).contains("csm migrate"));
        }
        assert!(parse(&os(&["go"])).is_err());
        assert!(parse(&os(&["--dry-run", "x"])).is_err());
    }

    #[test]
    fn exit_codes() {
        let nothing = Report::default();
        assert_eq!(exit_code(&nothing), 0);
        let pending = Report {
            legacy: true,
            pending: vec!["x".into()],
            ..Report::default()
        };
        assert_eq!(exit_code(&pending), EXIT_PENDING);
        let cut = Report {
            legacy: true,
            cutover_recorded: true,
            pending: vec!["retire waits for a reboot".into()],
            ..Report::default()
        };
        assert_eq!(exit_code(&cut), 0);
        let busy = Report {
            busy: true,
            legacy: true,
            ..Report::default()
        };
        assert_eq!(exit_code(&busy), EXIT_PENDING);
        // An error after the cutover is reported, but the setters may go.
        let late = Report {
            legacy: true,
            cutover_recorded: true,
            errors: vec!["~/.claude/plugins is a link outside ~/.claude.shared".into()],
            ..Report::default()
        };
        assert_eq!(exit_code(&late), 0);
        let err = Report {
            legacy: true,
            errors: vec!["cannot read profiles.json".into()],
            ..Report::default()
        };
        assert_eq!(exit_code(&err), 1);
    }
}
