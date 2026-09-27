//! `csm claude <args…>` — the documented passthrough verb.
//!
//! Everything after the word is handed to the claude binary verbatim: no csm
//! flag parsing, no session picker, no account scoring or auto-switch, no
//! sidecar, no pidfile, no relaunch loop. csm keeps no profiles, so there is
//! nothing to pin: the child runs in csm's runtime dir `D` (Orca's
//! `CLAUDE_CONFIG_DIR`, else `~/.claude`), and `CLAUDE_CONFIG_DIR` is set only
//! when the inherited value would put claude somewhere other than `D`
//! ([`crate::launch_context::config_dir_pin`]).
//!
//! That makes the verb the escape hatch for claude's own subcommands and
//! flags (`csm claude mcp list`, `csm claude --version`) without the
//! launcher's machinery in the way.
//! `claude` is not one of claude's own subcommand words, so reserving it does
//! not narrow what `csm <word>` can forward (see
//! `cli::reserved::CLAUDE_RESERVED_SUBCOMMANDS`).
//!
//! Unix replaces the process with `exec`, so claude owns the terminal outright
//! — signals, job control and the exit status are its own and csm is gone.
//! Windows has no exec: it spawns, waits, and exits with the child's code.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use crate::{config, launch_context};

/// What the passthrough will run: the full argv plus the `CLAUDE_CONFIG_DIR`
/// to pin for the child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Passthrough {
    /// `argv[0]` is the binary to execute; the rest are its arguments.
    pub(crate) argv: Vec<OsString>,
    /// Directory to export as `CLAUDE_CONFIG_DIR`, or `None` to leave the
    /// inherited environment untouched.
    pub(crate) config_dir: Option<PathBuf>,
}

/// `csm claude <args…>`
///
/// The I/O shell: resolve `D`, decide whether the child needs a
/// `CLAUDE_CONFIG_DIR` pin, then exec. Both decisions are pure functions
/// ([`launch_context::config_dir_pin`], [`plan`]) so they can be unit-tested
/// without a claude binary anywhere near the test.
pub(crate) fn cmd_claude(args: &[OsString]) -> anyhow::Result<()> {
    let plan = plan(
        &config::launch_command_for_spawn()?,
        launch_context::runtime_dir_pin(),
        args,
    );
    exec(&plan)
}

/// Pure core: the argv to execute.
///
/// `launch` is `config::resolve_launch_command`'s output — `launch[0]` is the
/// binary (`claude`, or the configured drop-in) and `launch[1..]` are tokens it
/// needs first (`npx happy`). The user's `args` follow, in order, untouched:
/// no flag of theirs is read, reordered, or dropped.
fn plan(launch: &[OsString], config_dir: Option<PathBuf>, args: &[OsString]) -> Passthrough {
    let mut argv: Vec<OsString> = Vec::with_capacity(launch.len() + args.len());
    argv.extend_from_slice(launch);
    argv.extend_from_slice(args);
    Passthrough { argv, config_dir }
}

/// Build the `Command` for a plan (shared by both `exec` impls).
fn command_for(plan: &Passthrough) -> anyhow::Result<Command> {
    let (bin, rest) = plan
        .argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("csm claude: empty launch command"))?;
    let mut cmd = Command::new(bin);
    cmd.args(rest);
    if let Some(dir) = &plan.config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    Ok(cmd)
}

/// Unix: replace this process with claude. Returns only on failure.
#[cfg(unix)]
fn exec(plan: &Passthrough) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt as _;

    let err = command_for(plan)?.exec();
    Err(anyhow::Error::new(err).context(format!(
        "csm claude: cannot execute {:?}",
        plan.argv[0].to_string_lossy()
    )))
}

/// Windows: no exec — spawn, wait, and exit with the child's code (non-zero
/// when it was killed by something that left no code).
#[cfg(not(unix))]
fn exec(plan: &Passthrough) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let status = command_for(plan)?.status().with_context(|| {
        format!(
            "csm claude: cannot execute {:?}",
            plan.argv[0].to_string_lossy()
        )
    })?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    // ── plan: args forwarded verbatim, after the launch tokens ────────────────

    #[test]
    fn plan_appends_args_after_the_launch_command() {
        let p = plan(&os(&["claude"]), None, &os(&["mcp", "list"]));
        assert_eq!(p.argv, os(&["claude", "mcp", "list"]));
        assert_eq!(p.config_dir, None);
    }

    #[test]
    fn plan_keeps_multi_token_launch_command_first() {
        // `csm config set launch-command npx happy` → the drop-in's own tokens
        // stay in front of the user's args.
        let p = plan(&os(&["npx", "happy"]), None, &os(&["--version"]));
        assert_eq!(p.argv, os(&["npx", "happy", "--version"]));
    }

    #[test]
    fn plan_forwards_csm_flags_verbatim_instead_of_reading_them() {
        // Every one of these is a flag `csm run` would consume. The
        // passthrough must not: they belong to claude here.
        let args = os(&["-c", "-i", "-n", "--", "-r"]);
        let p = plan(&os(&["claude"]), None, &args);
        assert_eq!(p.argv[1..], args[..]);
    }

    #[test]
    fn plan_with_no_args_is_just_the_launch_command() {
        let p = plan(&os(&["claude"]), None, &[]);
        assert_eq!(p.argv, os(&["claude"]));
    }

    #[test]
    fn plan_carries_the_config_dir_through() {
        let dir = PathBuf::from("/Users/example/.claude.work");
        let p = plan(&os(&["claude"]), Some(dir.clone()), &[]);
        assert_eq!(p.config_dir, Some(dir));
    }

    // ── command_for ───────────────────────────────────────────────────────────

    #[test]
    fn command_for_uses_argv0_as_the_binary() {
        let p = plan(&os(&["claude"]), None, &os(&["--version"]));
        let cmd = command_for(&p).expect("plan has a binary");
        assert_eq!(cmd.get_program(), OsString::from("claude").as_os_str());
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![OsString::from("--version").as_os_str()]);
    }

    #[test]
    fn command_for_empty_argv_is_an_error() {
        let p = Passthrough {
            argv: Vec::new(),
            config_dir: None,
        };
        assert!(command_for(&p).is_err());
    }
}
