//! Which invocations may migrate ([`trigger_class`]) and the cheap check
//! that decides whether there is anything to migrate ([`probe`]).
//!
//! Both are pure. The probe's shell ([`super::probe_now`]) gathers its facts
//! lazily: on a migrated machine it costs one small read (the marker) and
//! one stat (the registry), and it runs no `launchctl`, no Keychain and no
//! RPC until legacy is detected.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::launch_context::LaunchContext;

use super::state::{MigrationState, Phase};

// ─── trigger classes ──────────────────────────────────────────────────────────

/// What an invocation may do about the migration (design section 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerClass {
    /// The probe, then every stage whose gate passes.
    Full,
    /// The probe only, and at most one rate-limited stderr line.
    Note,
    /// Nothing before the spawn; a full run after it, log only.
    Pane,
    /// Nothing at all: no probe, no stage, no line.
    None,
}

/// The class of `csm <word> <rest…>`. `launch` is the launch context of a
/// `run` (explicit, implicit or the `claude` alias); `None` for `run`
/// means the dispatcher has not classified it yet, and the launch path
/// asks again once it has. Pure.
pub(crate) fn trigger_class(
    word: &str,
    rest: &[OsString],
    launch: Option<LaunchContext>,
) -> TriggerClass {
    let words: Vec<String> = rest
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let help = |w: &[&str]| w.iter().any(|a| matches!(*a, "-h" | "--help" | "help"));
    match word {
        "run" => match launch {
            Some(LaunchContext::Interactive) => TriggerClass::Full,
            Some(LaunchContext::OrcaPane | LaunchContext::OrcaStructured) => TriggerClass::Pane,
            Some(LaunchContext::Print) | None => TriggerClass::None,
        },
        "migrate" if help(&words) => TriggerClass::None,
        "migrate" => TriggerClass::Full,
        "orca" => match words.as_slice() {
            ["setup"] => TriggerClass::Full,
            [] | ["status"] => TriggerClass::Note,
            _ => TriggerClass::None,
        },
        "accounts" => {
            use crate::cmd::accounts::{AccountsCmd, parse};
            match parse(rest) {
                Ok(
                    AccountsCmd::Use(_)
                    | AccountsCmd::Add
                    | AccountsCmd::Import(_)
                    | AccountsCmd::Rm(_)
                    | AccountsCmd::Doctor { fix: true, .. },
                ) => TriggerClass::Full,
                Ok(AccountsCmd::List { .. } | AccountsCmd::Doctor { fix: false, .. }) => {
                    TriggerClass::Note
                }
                Ok(AccountsCmd::Help) | Err(_) => TriggerClass::None,
            }
        }
        "usage" if words.first() == Some(&"capture") => TriggerClass::None,
        "usage" if words.iter().any(|a| matches!(*a, "-h" | "--help")) => TriggerClass::None,
        "usage" => TriggerClass::Note,
        // hook, statusline, claude, cas, scan, sidecar, reap, completions,
        // newuuid, config, and anything main answers itself (--version,
        // --help).
        _ => TriggerClass::None,
    }
}

// ─── the probe ────────────────────────────────────────────────────────────────

/// What the probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The marker says done and the registry is gone: nothing to do.
    Done,
    /// No marker and nothing legacy: write a `done` marker.
    Fresh,
    /// Legacy is present, or the marker records a migration under way.
    Pending,
}

/// `~/.config/claude-as/profiles.json`.
pub(crate) fn registry_path(home: &Path) -> PathBuf {
    super::legacy::legacy_dir(home).join("profiles.json")
}

/// `~/.claude.shared`.
pub(crate) fn shared_path(home: &Path) -> PathBuf {
    home.join(".claude.shared")
}

/// Does an inherited `CLAUDE_CONFIG_DIR` name a `~/.claude.<name>` dir
/// (the legacy per-profile spelling)? `~/.claude.json` is a file and does
/// not count. Pure.
pub(crate) fn names_legacy_spelling(inherited: Option<&str>, home: &Path) -> bool {
    let Some(v) = inherited.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let p = Path::new(v.trim_end_matches(['/', '\\']));
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    p.parent() == Some(home) && name.starts_with(".claude.") && name != ".claude.json"
}

/// The cheap probe. `marker` is `<state>/migration.json` as read (`None`:
/// absent or corrupt); `exists` stats a path; `inherited` is this
/// process's `CLAUDE_CONFIG_DIR`. With a `done` marker it stats only the
/// registry. Pure over its inputs.
pub(crate) fn probe(
    marker: Option<&MigrationState>,
    exists: &dyn Fn(&Path) -> bool,
    inherited: Option<&str>,
    home: &Path,
) -> ProbeOutcome {
    match marker {
        Some(m) if m.phase == Phase::Done => {
            if exists(&registry_path(home)) {
                // The registry came back (a converge rendered it again):
                // adopt what it names.
                ProbeOutcome::Pending
            } else {
                ProbeOutcome::Done
            }
        }
        Some(_) => ProbeOutcome::Pending,
        None => {
            let legacy = exists(&registry_path(home))
                || exists(&shared_path(home))
                || names_legacy_spelling(inherited, home);
            if legacy {
                ProbeOutcome::Pending
            } else {
                ProbeOutcome::Fresh
            }
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsStr;

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    fn class(word: &str, rest: &[&str]) -> TriggerClass {
        trigger_class(word, &os(rest), None)
    }

    #[test]
    fn full_triggers() {
        assert_eq!(class("migrate", &[]), TriggerClass::Full);
        assert_eq!(class("migrate", &["--dry-run"]), TriggerClass::Full);
        assert_eq!(class("orca", &["setup"]), TriggerClass::Full);
        assert_eq!(
            trigger_class("run", &[], Some(LaunchContext::Interactive)),
            TriggerClass::Full
        );
        assert_eq!(class("accounts", &["use", "work"]), TriggerClass::Full);
        assert_eq!(class("accounts", &["add"]), TriggerClass::Full);
        assert_eq!(
            class("accounts", &["import", "/Users/example/.claude.work"]),
            TriggerClass::Full
        );
        assert_eq!(class("accounts", &["rm", "work"]), TriggerClass::Full);
        assert_eq!(class("accounts", &["doctor", "--fix"]), TriggerClass::Full);
    }

    #[test]
    fn note_triggers() {
        assert_eq!(class("accounts", &[]), TriggerClass::Note);
        assert_eq!(class("accounts", &["list"]), TriggerClass::Note);
        assert_eq!(class("accounts", &["doctor"]), TriggerClass::Note);
        assert_eq!(class("usage", &[]), TriggerClass::Note);
        assert_eq!(class("usage", &["--json"]), TriggerClass::Note);
        assert_eq!(class("orca", &[]), TriggerClass::Note);
        assert_eq!(class("orca", &["status"]), TriggerClass::Note);
    }

    #[test]
    fn pane_triggers() {
        for launch in [LaunchContext::OrcaPane, LaunchContext::OrcaStructured] {
            assert_eq!(trigger_class("run", &[], Some(launch)), TriggerClass::Pane);
        }
    }

    /// Every NONE row of design section 1, and help or a bad parse of a
    /// FULL/NOTE word.
    #[test]
    fn none_triggers() {
        for word in [
            "hook",
            "statusline",
            "claude",
            "cas",
            "scan",
            "sidecar",
            "reap",
            "completions",
            "newuuid",
            "config",
            "--version",
            "--help",
        ] {
            assert_eq!(class(word, &[]), TriggerClass::None, "{word}");
        }
        assert_eq!(class("usage", &["capture"]), TriggerClass::None);
        assert_eq!(class("usage", &["--help"]), TriggerClass::None);
        assert_eq!(
            trigger_class("run", &os(&["-p", "hi"]), Some(LaunchContext::Print)),
            TriggerClass::None
        );
        // The dispatcher's view of a launch: decided later by the launch.
        assert_eq!(class("run", &[]), TriggerClass::None);
        assert_eq!(class("accounts", &["help"]), TriggerClass::None);
        assert_eq!(class("accounts", &["use"]), TriggerClass::None);
        assert_eq!(class("migrate", &["--help"]), TriggerClass::None);
        assert_eq!(class("orca", &["--help"]), TriggerClass::None);
    }

    /// A Print launch (`-p`, `--print`, a piped stdin; as `csm`, as the
    /// `claude` alias, inside an Orca pane too) classifies as Print, and a
    /// Print `run` is NONE: the two halves `csm run` composes before any
    /// migration call. `cmd_run` returns on Print before it classifies at
    /// all (its I/O path is covered by the e2e `sc_auto_untouched`).
    #[test]
    fn print_launches_never_migrate() {
        use crate::launch_context::launch_context;
        let plain = |_: &str| None::<String>;
        let pane = |k: &str| (k == "ORCA_PANE_KEY").then(|| "pane-1".to_owned());
        let cases: &[(&str, &[&str], bool)] = &[
            ("csm", &["-p", "hi"], true),
            ("csm", &["--print", "hi"], true),
            ("csm", &[], false),
            ("claude", &["-p", "hi"], true),
            ("claude", &[], false),
        ];
        for (argv0, argv, tty) in cases {
            for get in [&plain as &dyn Fn(&str) -> Option<String>, &pane] {
                let argv = os(argv);
                let l = launch_context(get, OsStr::new(argv0), &argv, *tty);
                assert_eq!(l.context, LaunchContext::Print, "{argv0} {argv:?} {tty}");
                assert_eq!(
                    trigger_class("run", &argv, Some(l.context)),
                    TriggerClass::None,
                    "{argv0} {argv:?}"
                );
            }
        }
    }

    /// A stat over a fake filesystem holding paths ending in `present`.
    fn stats(present: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p: &Path| present.iter().any(|s| p.ends_with(s))
    }

    #[test]
    fn a_migrated_machine_costs_one_stat() {
        let home = Path::new("/Users/example");
        let done = MigrationState::done();
        let seen = RefCell::new(Vec::new());
        let present = stats(&[]);
        let exists = |p: &Path| {
            seen.borrow_mut().push(p.to_path_buf());
            present(p)
        };
        assert_eq!(
            probe(
                Some(&done),
                &exists,
                Some("/Users/example/.claude.work"),
                home
            ),
            ProbeOutcome::Done
        );
        assert_eq!(*seen.borrow(), vec![registry_path(home)]);
    }

    #[test]
    fn a_registry_back_after_done_is_pending() {
        let home = Path::new("/Users/example");
        let present = stats(&["profiles.json"]);
        assert_eq!(
            probe(Some(&MigrationState::done()), &present, None, home),
            ProbeOutcome::Pending
        );
    }

    #[test]
    fn a_marker_under_way_is_pending_without_a_stat() {
        let home = Path::new("/Users/example");
        let seen = RefCell::new(0);
        let exists = |_: &Path| {
            *seen.borrow_mut() += 1;
            false
        };
        let m = MigrationState::default();
        assert_eq!(probe(Some(&m), &exists, None, home), ProbeOutcome::Pending);
        assert_eq!(*seen.borrow(), 0);
    }

    #[test]
    fn no_marker_detects_each_legacy_signal() {
        let home = Path::new("/Users/example");
        let none = stats(&[]);
        assert_eq!(probe(None, &none, None, home), ProbeOutcome::Fresh);
        assert_eq!(
            probe(None, &none, Some("/Users/example/.claude"), home),
            ProbeOutcome::Fresh
        );
        assert_eq!(
            probe(None, &none, Some("/Users/example/.claude.json"), home),
            ProbeOutcome::Fresh
        );
        assert_eq!(
            probe(None, &none, Some("/Volumes/x/.claude.work"), home),
            ProbeOutcome::Fresh
        );
        let reg = stats(&["profiles.json"]);
        assert_eq!(probe(None, &reg, None, home), ProbeOutcome::Pending);
        let shared = stats(&[".claude.shared"]);
        assert_eq!(probe(None, &shared, None, home), ProbeOutcome::Pending);
        assert_eq!(
            probe(None, &none, Some("/Users/example/.claude.home/"), home),
            ProbeOutcome::Pending
        );
    }
}
