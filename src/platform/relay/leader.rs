//! The pty leader: a re-exec of csm itself (`__pty-leader <slave-path> --
//! <argv...>`), dispatched in `main` before any other argv handling. It is
//! the session leader on a *second*, inner pty (the one `RelayLauncher`
//! allocated) and does the actual job-control work — `setsid`, acquiring the
//! slave as its controlling terminal, spawning claude with the slave as its
//! stdio, and handing the slave's foreground process group to claude. The
//! outer `csm` process (the "relay") never touches this inner pty directly;
//! it only exchanges line-based protocol messages with the leader over a
//! piped stdin/stdout, and relays raw bytes between the *outer* real
//! terminal and the *master* side of the inner pty.
//!
//! ## Protocol
//!
//! Leader → relay (its stdout, one line per event, always flushed):
//!   - `pid <n>` — claude's real pid. Reported exactly once, synchronously,
//!     right after spawn, before anything else. The relay blocks on this one
//!     line before doing anything else (pidfile, `on_spawn()`, worker
//!     threads); if the leader's stdout closes before this line arrives, the
//!     relay treats it as a setup failure and falls back to the direct
//!     launcher for that hop.
//!   - `stopped` — claude's process group was stopped (`WIFSTOPPED`).
//!   - `exit <code>` — claude exited normally.
//!   - `signal <n>` — claude was killed by this signal.
//!
//! Relay → leader (its stdin, one line per command):
//!   - `cont` — resume claude's process group (`SIGCONT`) after a `stopped`
//!     report; sent once the relay has re-entered raw mode and resynced the
//!     window size.
//!   - `signal <n>` — forward this signal to claude's process group (used to
//!     propagate `SIGTERM`/`SIGHUP`/`SIGINT` received by the relay itself).
//!
//! On EOF on its stdin (the relay died or closed the pipe), the leader
//! forwards `SIGHUP` to claude's process group and exits once claude is
//! gone — the same "hang up the child when the controlling session goes
//! away" semantics a real terminal driver gives you for free, reconstructed
//! here because the leader's controlling terminal is the *inner* pty, not
//! the real one.
//!
//! The leader's own stderr is piped back to the relay (never inherited from
//! the real terminal): the relay appends each line to `idle-compact.log`
//! rather than ever letting leader diagnostics touch the raw outer screen.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;

use nix::fcntl::{OFlag, open};
use nix::sys::signal::{Signal, killpg};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, setpgid, setsid};

use super::set_controlling_tty;

/// One report line the leader writes to its stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderReport {
    /// `pid <n>`.
    Pid(u32),
    /// `stopped`.
    Stopped,
    /// `exit <code>`.
    Exit(i32),
    /// `signal <n>`.
    Signal(i32),
}

impl LeaderReport {
    pub fn format(self) -> String {
        match self {
            LeaderReport::Pid(n) => format!("pid {n}"),
            LeaderReport::Stopped => "stopped".to_string(),
            LeaderReport::Exit(code) => format!("exit {code}"),
            LeaderReport::Signal(sig) => format!("signal {sig}"),
        }
    }

    /// Parse one line (trailing newline already stripped or not, both fine).
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line == "stopped" {
            return Some(LeaderReport::Stopped);
        }
        let (word, rest) = line.split_once(' ')?;
        let n: i64 = rest.trim().parse().ok()?;
        match word {
            "pid" => u32::try_from(n).ok().map(LeaderReport::Pid),
            "exit" => i32::try_from(n).ok().map(LeaderReport::Exit),
            "signal" => i32::try_from(n).ok().map(LeaderReport::Signal),
            _ => None,
        }
    }
}

/// One command line the relay writes to the leader's stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderCommand {
    /// `cont`.
    Cont,
    /// `signal <n>`.
    Signal(i32),
}

impl LeaderCommand {
    pub fn format(self) -> String {
        match self {
            LeaderCommand::Cont => "cont".to_string(),
            LeaderCommand::Signal(sig) => format!("signal {sig}"),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line == "cont" {
            return Some(LeaderCommand::Cont);
        }
        let (word, rest) = line.split_once(' ')?;
        if word != "signal" {
            return None;
        }
        rest.trim().parse::<i32>().ok().map(LeaderCommand::Signal)
    }
}

/// Write one report line to the leader's own stdout and flush it immediately
/// (the relay is blocked reading a line at a time; an unflushed report would
/// hang it).
fn report(r: LeaderReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{}", r.format());
    let _ = out.flush();
}

fn log_err(msg: impl std::fmt::Display) {
    eprintln!("pty-leader: {msg}");
}

/// Entry point for `csm __pty-leader <slave-path> -- <argv...>`. `args` is
/// everything after `__pty-leader` itself. Returns the process exit code;
/// never panics on a malformed environment — logs to stderr (piped back to
/// the relay's log, never the raw terminal) and returns non-zero instead.
pub fn main(args: &[OsString]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            log_err(e);
            1
        }
    }
}

fn run(args: &[OsString]) -> io::Result<()> {
    let slave_path = args
        .first()
        .ok_or_else(|| io::Error::other("__pty-leader: missing slave path argument"))?;
    let sep = args
        .iter()
        .position(|a| a == "--")
        .ok_or_else(|| io::Error::other("__pty-leader: missing -- separator"))?;
    let child_argv = &args[sep + 1..];
    let (bin, rest) = child_argv
        .split_first()
        .ok_or_else(|| io::Error::other("__pty-leader: missing child argv"))?;
    let slave_path = std::path::Path::new(slave_path);

    // New session: this process becomes the session leader with no
    // controlling terminal yet.
    setsid().map_err(io::Error::from)?;

    // Open the slave and make it our controlling terminal. O_NOCTTY on the
    // open itself (we acquire it explicitly via TIOCSCTTY right after, not
    // implicitly on open).
    let slave = open(slave_path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
        .map_err(io::Error::from)?;
    set_controlling_tty(&slave)?;

    // Give claude three independent fds onto the slave (its stdin/stdout/
    // stderr): reopen the path rather than dup the one fd above, so we can
    // drop `slave` once spawn returns and hold nothing but claude's own fds
    // open on the inner pty.
    let child_stdin = open(slave_path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
        .map_err(io::Error::from)?;
    let child_stdout = open(slave_path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
        .map_err(io::Error::from)?;
    let child_stderr = open(slave_path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
        .map_err(io::Error::from)?;

    let mut cmd = Command::new(bin);
    cmd.args(rest);
    cmd.stdin(Stdio::from(child_stdin));
    cmd.stdout(Stdio::from(child_stdout));
    cmd.stderr(Stdio::from(child_stderr));
    // Claude gets its own process group (not the leader's) so job-control
    // signals (SIGTSTP/SIGTTIN/SIGTTOU) generated on the inner pty target
    // claude, not the leader.
    unsafe {
        cmd.pre_exec(|| {
            let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
            // Take the foreground before exec instead of waiting for the
            // parent's `tcsetpgrp` below. Until that lands claude is a
            // background job, and a `tcsetattr` (or a write under TOSTOP)
            // in its first milliseconds raises SIGTTOU/SIGTTIN and stops it
            // for good, which left the session hung on loaded CI runners.
            // SIGTTOU is ignored only for this call and the previous
            // disposition restored so nothing leaks through exec.
            // `libc` calls only: this runs between fork and exec.
            let mut ign: libc::sigaction = std::mem::zeroed();
            ign.sa_sigaction = libc::SIG_IGN;
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGTTOU, &ign, &mut old);
            let _ = libc::tcsetpgrp(0, libc::getpgrp());
            libc::sigaction(libc::SIGTTOU, &old, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let child_pgid = Pid::from_raw(pid as i32);
    // Race-free: whichever of parent/child wins the setpgid call, the child
    // ends up in its own group before anyone acts on it.
    let _ = setpgid(child_pgid, child_pgid);
    // The leader itself is the sole member of its own pgrp at this point
    // (it just became the inner pty's foreground group via TIOCSCTTY), so
    // this tcsetpgrp call is made *by* the foreground group and cannot
    // raise SIGTTOU — no ignore-around-it dance needed, unlike posix.rs's
    // supervisor-to-child handoff on the outer terminal.
    let _ = nix::unistd::tcsetpgrp(&slave, child_pgid);
    drop(slave);

    report(LeaderReport::Pid(pid));

    // Reader thread: parse commands from our own stdin and act immediately.
    // On EOF (the relay died or closed the pipe), forward SIGHUP and stop.
    let cmd_pgid = child_pgid;
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            match LeaderCommand::parse(&line) {
                Some(LeaderCommand::Cont) => {
                    let _ = killpg(cmd_pgid, Signal::SIGCONT);
                }
                Some(LeaderCommand::Signal(n)) => {
                    if let Ok(sig) = Signal::try_from(n) {
                        let _ = killpg(cmd_pgid, sig);
                    }
                }
                None => {}
            }
        }
        // SIGCONT too: a stopped group would otherwise sit on the pending HUP.
        let _ = killpg(cmd_pgid, Signal::SIGHUP);
        let _ = killpg(cmd_pgid, Signal::SIGCONT);
    });

    // Main loop: wait for claude's state changes and report each one. Exits
    // once claude has definitively exited or been killed by a signal.
    let target = Pid::from_raw(pid as i32);
    loop {
        match waitpid(target, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Exited(_, code)) => {
                report(LeaderReport::Exit(code));
                break;
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                report(LeaderReport::Signal(sig as i32));
                break;
            }
            Ok(WaitStatus::Stopped(_, _)) => {
                report(LeaderReport::Stopped);
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::from(e)),
        }
    }
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_report_roundtrip() {
        assert_eq!(
            LeaderReport::parse("pid 1234"),
            Some(LeaderReport::Pid(1234))
        );
        assert_eq!(LeaderReport::Pid(1234).format(), "pid 1234");
    }

    #[test]
    fn stopped_report_roundtrip() {
        assert_eq!(LeaderReport::parse("stopped"), Some(LeaderReport::Stopped));
        assert_eq!(
            LeaderReport::parse(" stopped \n"),
            Some(LeaderReport::Stopped)
        );
        assert_eq!(LeaderReport::Stopped.format(), "stopped");
    }

    #[test]
    fn exit_report_roundtrip() {
        assert_eq!(LeaderReport::parse("exit 0"), Some(LeaderReport::Exit(0)));
        assert_eq!(
            LeaderReport::parse("exit 130"),
            Some(LeaderReport::Exit(130))
        );
        assert_eq!(LeaderReport::Exit(7).format(), "exit 7");
    }

    #[test]
    fn signal_report_roundtrip() {
        assert_eq!(
            LeaderReport::parse("signal 9"),
            Some(LeaderReport::Signal(9))
        );
        assert_eq!(LeaderReport::Signal(15).format(), "signal 15");
    }

    #[test]
    fn report_rejects_garbage() {
        assert_eq!(LeaderReport::parse(""), None);
        assert_eq!(LeaderReport::parse("nonsense"), None);
        assert_eq!(LeaderReport::parse("pid abc"), None);
        assert_eq!(LeaderReport::parse("exit"), None);
    }

    #[test]
    fn cont_command_roundtrip() {
        assert_eq!(LeaderCommand::parse("cont"), Some(LeaderCommand::Cont));
        assert_eq!(LeaderCommand::Cont.format(), "cont");
    }

    #[test]
    fn signal_command_roundtrip() {
        assert_eq!(
            LeaderCommand::parse("signal 1"),
            Some(LeaderCommand::Signal(1))
        );
        assert_eq!(LeaderCommand::Signal(2).format(), "signal 2");
    }

    #[test]
    fn command_rejects_garbage() {
        assert_eq!(LeaderCommand::parse(""), None);
        assert_eq!(LeaderCommand::parse("pid 1"), None);
        assert_eq!(LeaderCommand::parse("signal abc"), None);
    }
}
