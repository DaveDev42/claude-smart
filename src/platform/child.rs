//! Bounded child-process handling: every run-with-timeout helper in the
//! crate goes through here so a stuck child can never hang csm (or a test).
//!
//! Rules this module encodes:
//! - A non-interactive child that may outlive its deadline is started in its
//!   own process group ([`own_group`]) so a timeout kill reaches its
//!   grandchildren too (`sh -c 'sleep 30 | cat'`). A child that needs the
//!   terminal (an interactive login) stays in csm's group: a background group
//!   reading the tty would be stopped by `SIGTTIN`.
//! - A kill is followed by a BOUNDED reap ([`reap_within`]): `wait()` after
//!   `kill()` can block for as long as the kernel keeps the child in exec
//!   (a stalled executable scanner has done exactly that). csm polls
//!   `try_wait` for at most [`REAP_LIMIT`] and then gives up; an unreaped
//!   child is a zombie until csm exits, never a hang.
//! - A pid read from a file or a table is only signalled after
//!   [`signal_pid`] accepts it: `0` would signal csm's own process group and
//!   anything above `i32::MAX` wraps negative (`-1` signals every process the
//!   user owns).

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// How long a kill waits for the child to be reaped before giving up.
pub const REAP_LIMIT: Duration = Duration::from_secs(2);

/// How often [`wait_deadline`] and [`reap_within`] poll `try_wait`.
pub const POLL: Duration = Duration::from_millis(10);

// ─── pure ─────────────────────────────────────────────────────────────────────

/// `pid` as a signal target, or `None` when signalling it would reach more
/// than one process: `0` (the caller's own process group) or a value above
/// `i32::MAX` (negative after the cast, i.e. a group or every process). Pure.
pub fn signal_pid(pid: u32) -> Option<i32> {
    match i32::try_from(pid) {
        Ok(p) if p > 0 => Some(p),
        _ => None,
    }
}

// ─── spawn / kill / reap ─────────────────────────────────────────────────────

/// Start the child as the leader of a new process group (unix; a no-op
/// elsewhere), so [`kill_tree`] with `grouped = true` reaches everything it
/// starts. Never use it for a child that reads the terminal.
pub fn own_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd
}

/// Keeps csm alive through a terminal interrupt while an interactive child
/// (one that stays in csm's process group) runs, so the cleanup after it
/// still happens. On unix it installs a no-op handler, not `SIG_IGN`, for
/// `SIGINT` and `SIGHUP`: a caught signal is reset to its default on
/// `exec`, so the child still gets Ctrl-C and a closed terminal and exits,
/// while csm only sees its read or wait interrupted (`SA_RESTART`) and goes
/// on. [`DeferInterrupts::take`] (or the drop) puts the previous handlers
/// back. A no-op elsewhere. `SIGTERM` and `SIGKILL` are not deferred.
pub struct DeferInterrupts {
    held: bool,
}

#[cfg(unix)]
extern "C" fn defer_interrupt_noop(_: std::ffi::c_int) {}

/// Guards alive, and the handlers the first one replaced. Counted so that
/// overlapping guards (threads, tests) never leave the no-op installed.
#[cfg(unix)]
type SavedHandlers = Vec<(nix::sys::signal::Signal, nix::sys::signal::SigAction)>;
#[cfg(unix)]
static DEFERRED: std::sync::Mutex<(usize, SavedHandlers)> = std::sync::Mutex::new((0, Vec::new()));

impl DeferInterrupts {
    pub fn install() -> DeferInterrupts {
        #[cfg(unix)]
        {
            use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
            let mut g = DEFERRED.lock().unwrap_or_else(|e| e.into_inner());
            if g.0 == 0 {
                let act = SigAction::new(
                    SigHandler::Handler(defer_interrupt_noop),
                    SaFlags::SA_RESTART,
                    SigSet::empty(),
                );
                g.1.clear();
                for sig in [Signal::SIGINT, Signal::SIGHUP] {
                    // SAFETY: the handler is async-signal-safe (it does nothing).
                    if let Ok(old) = unsafe { sigaction(sig, &act) } {
                        g.1.push((sig, old));
                    }
                }
            }
            g.0 += 1;
        }
        DeferInterrupts { held: true }
    }

    /// Release this guard now; the last one alive puts the previous
    /// handlers back. Idempotent.
    pub fn take(&mut self) {
        let held = std::mem::take(&mut self.held);
        #[cfg(unix)]
        if held {
            let mut g = DEFERRED.lock().unwrap_or_else(|e| e.into_inner());
            g.0 = g.0.saturating_sub(1);
            if g.0 == 0 {
                for (sig, old) in g.1.drain(..) {
                    // SAFETY: restores the disposition `install` read.
                    let _ = unsafe { nix::sys::signal::sigaction(sig, &old) };
                }
            }
        }
        #[cfg(not(unix))]
        let _ = held;
    }
}

impl Drop for DeferInterrupts {
    fn drop(&mut self) {
        self.take();
    }
}

/// SIGKILL the child and, when it was started with [`own_group`]
/// (`grouped`), its whole process group. Safe while the child is unreaped:
/// its pid, and so its group id, cannot be reused before the reap.
pub fn kill_tree(child: &mut Child, grouped: bool) {
    #[cfg(unix)]
    if grouped && let Some(pgid) = signal_pid(child.id()) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = grouped;
    let _ = child.kill();
}

/// SIGKILL the process group `pgid` that a child started with
/// [`own_group`] led. For the case where the leader already exited and was
/// reaped but its group lives on (a background job holding a pipe): while
/// any member remains, the id cannot be handed to a new process.
pub fn kill_group(pgid: u32) {
    #[cfg(unix)]
    if let Some(pgid) = signal_pid(pgid) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Poll `try_wait` for at most `limit`. `None` when the child is still not
/// reaped (or `try_wait` failed): the caller gives up rather than block.
pub fn reap_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => return None,
        }
    }
}

/// Kill (see [`kill_tree`]) and reap within [`REAP_LIMIT`].
pub fn kill_and_reap(child: &mut Child, grouped: bool) -> Option<ExitStatus> {
    kill_tree(child, grouped);
    reap_within(child, REAP_LIMIT)
}

/// Wait for the child to exit within `timeout`.
///
/// - `Ok(Some(status))`: it exited;
/// - `Ok(None)`: the deadline passed; the child (and its group when
///   `grouped`) was killed and a bounded reap attempted;
/// - `Err(e)`: `try_wait` failed; the child was killed the same way.
pub fn wait_deadline(
    child: &mut Child,
    timeout: Duration,
    poll: Duration,
    grouped: bool,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Ok(Some(s)),
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(child, grouped);
                return Ok(None);
            }
            Ok(None) => std::thread::sleep(poll),
            Err(e) => {
                kill_and_reap(child, grouped);
                return Err(e);
            }
        }
    }
}

// ─── test fixture ─────────────────────────────────────────────────────────────

/// A spawned child a test owns: killed (with its group when `grouped`) and
/// reaped within [`REAP_LIMIT`] on drop, so a panicking assertion never
/// leaves a process behind.
#[cfg(test)]
pub(crate) struct ChildGuard {
    child: Option<Child>,
    grouped: bool,
}

#[cfg(test)]
impl ChildGuard {
    /// Spawn `cmd` in its own process group and guard it.
    pub(crate) fn spawn(cmd: &mut Command) -> io::Result<ChildGuard> {
        let child = own_group(cmd).spawn()?;
        Ok(ChildGuard {
            child: Some(child),
            grouped: true,
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    /// Kill and reap now; `true` when the child was reaped.
    pub(crate) fn stop(&mut self) -> bool {
        match self.child.take() {
            Some(mut c) => kill_and_reap(&mut c, self.grouped).is_some(),
            None => true,
        }
    }
}

#[cfg(test)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_pid_rejects_zero_and_values_that_wrap_negative() {
        assert_eq!(signal_pid(0), None);
        assert_eq!(signal_pid(u32::MAX), None, "would be kill(-1)");
        assert_eq!(signal_pid(i32::MAX as u32 + 1), None);
        assert_eq!(signal_pid(1), Some(1));
        assert_eq!(signal_pid(4242), Some(4242));
        assert_eq!(signal_pid(i32::MAX as u32), Some(i32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn a_deadline_kills_the_whole_group_and_reaps() {
        // `sh -c '/bin/sleep 5 | /bin/cat'`: the pipeline's processes are
        // grandchildren; only a group kill reaches them.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "/bin/sleep 5 | /bin/cat"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = own_group(&mut cmd).spawn().unwrap();
        let pgid = child.id() as i32;
        let start = Instant::now();
        let got = wait_deadline(&mut child, Duration::from_millis(200), POLL, true).unwrap();
        assert!(got.is_none());
        assert!(start.elapsed() < Duration::from_secs(4));
        // No process is left in the group.
        let gone = (0..200).any(|_| {
            let r = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None);
            if r == Err(nix::errno::Errno::ESRCH) {
                return true;
            }
            std::thread::sleep(POLL);
            false
        });
        assert!(gone, "the process group must be empty after the kill");
    }

    #[cfg(unix)]
    #[test]
    fn an_exited_child_is_reported() {
        let mut child = own_group(Command::new("/bin/sh").args(["-c", "exit 3"]))
            .spawn()
            .unwrap();
        let st = wait_deadline(&mut child, Duration::from_secs(5), POLL, true)
            .unwrap()
            .unwrap();
        assert_eq!(st.code(), Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn the_guard_kills_and_reaps_on_drop() {
        let mut g = ChildGuard::spawn(Command::new("/bin/sleep").arg("5")).unwrap();
        let pid = g.id() as i32;
        assert!(g.stop(), "the guard reaps its child");
        let r = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        assert_eq!(r, Err(nix::errno::Errno::ESRCH));
    }

    /// The login's interrupt guard: while any guard lives SIGINT and SIGHUP
    /// run a handler (reset on `exec`, so children still get them), and
    /// the last guard puts the default back.
    #[cfg(unix)]
    #[test]
    fn deferred_interrupts_are_counted_and_restored() {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        fn current(sig: Signal) -> SigHandler {
            let probe = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
            // SAFETY: reads the disposition and puts it straight back.
            let old = unsafe { sigaction(sig, &probe) }.unwrap();
            unsafe { sigaction(sig, &old) }.unwrap();
            old.handler()
        }
        let mut a = DeferInterrupts::install();
        let b = DeferInterrupts::install();
        {
            let _g = DEFERRED.lock().unwrap();
            for sig in [Signal::SIGINT, Signal::SIGHUP] {
                assert!(matches!(current(sig), SigHandler::Handler(_)), "{sig:?}");
            }
        }
        a.take();
        a.take();
        {
            let _g = DEFERRED.lock().unwrap();
            assert!(matches!(current(Signal::SIGINT), SigHandler::Handler(_)));
        }
        drop(b);
        let g = DEFERRED.lock().unwrap();
        // Another test's login may hold a guard; only a count of 0 says
        // the handlers must be back.
        if g.0 == 0 {
            for sig in [Signal::SIGINT, Signal::SIGHUP] {
                assert!(!matches!(current(sig), SigHandler::Handler(_)), "{sig:?}");
            }
        }
    }
}
