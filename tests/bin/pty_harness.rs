//! Test-only process that plays "the shell": becomes the session leader of a
//! given pty slave (the *outer* terminal `tests/pty_relay.rs` drives csm
//! under) and hands it, as a real job-control job, to `program`. Never built
//! into a release artifact.
//!
//! Usage: `pty_harness <slave-path> <program> [args...]`
//! (env `PTY_HARNESS_SIZE=<rows>x<cols>` sets the window size first)
//!
//! This is a *separate OS process* (spawned by the test via
//! `std::process::Command`) specifically so its `setsid()` call cannot race
//! another concurrently-running `#[test]` in the same test binary — session
//! state is process-wide, and `cargo test` runs multiple tests as threads of
//! one process.
//!
//! `program` (csm) is spawned as a genuine *child* job, in its own process
//! group, with this process staying alive in the session as its "shell" —
//! deliberately not an exec-in-place (same pid) handoff, and this is not a
//! style choice: a lone session leader with no live parent left inside its
//! own session is, by definition, an *orphaned* process group, and on this
//! platform (confirmed by direct measurement) a stop-class signal
//! (`SIGTSTP`) delivered to a member of an orphaned group is silently
//! discarded by the kernel rather than actually stopping it — which would
//! silently break `ctrl_z_suspends_and_sigcont_resumes` no matter what the
//! relay's own signal-handling code does, since csm's self-suspend
//! (`leader_control_thread`'s `raise(Signal::SIGTSTP)`, see
//! `src/platform/relay/mod.rs`) depends on its own process group actually
//! being stoppable. This process staying alive (blocked in `wait_for` below)
//! for as long as csm runs — including while csm is genuinely stopped — is
//! exactly what keeps csm's group from being orphaned, mirroring how a real
//! interactive shell (which never exits while a foreground job is running or
//! stopped) keeps a launched program's group non-orphaned in real usage. See
//! `src/platform/posix.rs`'s own supervisor-to-child handoff for the same
//! `setpgid`/`tcsetpgrp`/`SIGTTOU`-around-the-handoff pattern, reused here
//! near-verbatim; `src/platform/relay/leader.rs` does the analogous handoff
//! for the *inner* pty.
//!
//! Reports csm's real pid to the test as `PID <n>\n` on this process's own
//! stdout (a plain pipe, never touched by csm's own stdio wiring below,
//! which only ever targets `program`'s fds) before the terminal handoff —
//! `tests/pty_relay.rs`'s `Session::pid()` reads this once, since csm is no
//! longer the same OS process as this harness.

#[cfg(unix)]
fn main() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    use nix::unistd::{Pid, setpgid, tcsetpgrp};

    let args: Vec<String> = std::env::args().collect();
    let slave_path = args
        .get(1)
        .expect("usage: pty_harness <slave-path> <program> [args...]");
    let program = args
        .get(2)
        .expect("usage: pty_harness <slave-path> <program> [args...]");
    let rest = &args[3..];

    nix::unistd::setsid().expect("pty_harness: setsid");

    let slave = nix::fcntl::open(
        Path::new(slave_path),
        nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
        nix::sys::stat::Mode::empty(),
    )
    .expect("pty_harness: open slave");

    let rc = unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) };
    assert_eq!(
        rc,
        0,
        "pty_harness: TIOCSCTTY failed: {}",
        std::io::Error::last_os_error()
    );

    // Optional: `PTY_HARNESS_SIZE=<rows>x<cols>` sets the terminal's window
    // size before csm starts, so a test that replays fixed-size screens sees
    // that size from the first byte (the relay reads it once at start).
    if let Some((rows, cols)) = std::env::var("PTY_HARNESS_SIZE")
        .ok()
        .as_deref()
        .and_then(|v| v.split_once('x'))
        .and_then(|(r, c)| Some((r.parse::<u16>().ok()?, c.parse::<u16>().ok()?)))
    {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
    }

    // Three independent fds onto the slave for csm's stdio (reopen the path
    // rather than dup `slave`, so `slave` itself can be dropped once the
    // job-control handoff below is done — same approach leader.rs uses for
    // claude on the inner pty).
    let open_slave = || {
        nix::fcntl::open(
            Path::new(slave_path),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        )
        .expect("pty_harness: open slave (child fd)")
    };
    let child_stdin = open_slave();
    let child_stdout = open_slave();
    let child_stderr = open_slave();

    let mut cmd = std::process::Command::new(program);
    cmd.args(rest);
    cmd.stdin(std::process::Stdio::from(child_stdin));
    cmd.stdout(std::process::Stdio::from(child_stdout));
    cmd.stderr(std::process::Stdio::from(child_stderr));
    // csm gets its own process group — not this harness's — so it is a real,
    // non-orphaned job (see the module doc) rather than a lone session
    // leader. setpgid(0,0), NOT setsid: csm must keep the controlling tty
    // this process just acquired, not drop it.
    // The harness itself ignores SIGHUP (it is the session leader, so a
    // terminal hangup would otherwise kill it and hide csm's exit status);
    // csm gets the default disposition back.
    let ign_hup = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGHUP, &ign_hup) }.expect("pty_harness: ignore SIGHUP");
    unsafe {
        cmd.pre_exec(|| {
            let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
            // Take the foreground ourselves, before exec, like a real shell's
            // forked child does. Waiting for the parent's `tcsetpgrp` below
            // races csm: if csm touches the terminal (tcsetattr, a write
            // under TOSTOP) first, it is a background job and SIGTTOU stops
            // it for good (the harness only reports STOPPED). Linux runners
            // with few CPUs lose that race; an idle macOS box rarely does.
            // SIGTTOU is ignored only across this call, and reset to the
            // default so it is not inherited through exec.
            let ign_ttou = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
            let dfl_ttou = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
            let _ = sigaction(Signal::SIGTTOU, &ign_ttou);
            let _ = libc::tcsetpgrp(0, libc::getpgrp());
            let _ = sigaction(Signal::SIGTTOU, &dfl_ttou);
            let dfl = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
            let _ = sigaction(Signal::SIGHUP, &dfl);
            Ok(())
        });
    }

    // Reaped below with a raw waitpid (WUNTRACED | WCONTINUED), not Child::wait.
    #[allow(clippy::zombie_processes)]
    let child = cmd.spawn().expect("pty_harness: spawn program");
    let pid = child.id();
    let child_pgid = Pid::from_raw(pid as i32);
    // Race-free with the child's own pre_exec setpgid: whoever wins, csm
    // ends up in its own group before anyone acts on it.
    let _ = setpgid(child_pgid, child_pgid);

    {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "PID {pid}");
        let _ = out.flush();
    }

    // Hand the terminal to csm's pgrp. This process is not yet in the
    // foreground group it is about to grant away, so the tcsetpgrp call
    // itself would raise SIGTTOU against it — ignore that signal only
    // across the handoff, exactly as posix.rs's own supervisor-to-child
    // handoff does for the real outer terminal.
    let ign = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    let saved = unsafe { sigaction(Signal::SIGTTOU, &ign) }.ok();
    let _ = tcsetpgrp(&slave, child_pgid);
    if let Some(prev) = saved {
        let _ = unsafe { sigaction(Signal::SIGTTOU, &prev) };
    }
    drop(slave);

    // Block until csm actually terminates — not merely stops: no WUNTRACED,
    // so a stopped (`T`-state) csm leaves this wait blocked exactly as a
    // real shell's job-control `wait()` would. This process staying alive
    // and blocked here, for as long as csm runs (including while genuinely
    // stopped), is what keeps csm's process group from being orphaned; see
    // the module doc.
    // Job-control view of csm, like a shell's: reports each stop and
    // continue as `STOPPED` / `CONTINUED` lines on stdout (waitpid with
    // WUNTRACED | WCONTINUED), so the test observes suspend and resume
    // without relying on `ps` state letters, which macOS leaves stale ("T")
    // after a multi-threaded process resumes.
    let pid = child.id() as libc::pid_t;
    loop {
        let mut st: libc::c_int = 0;
        let rc = unsafe {
            libc::waitpid(
                pid,
                &mut st,
                libc::WUNTRACED | libc::WCONTINUED | libc::WNOHANG,
            )
        };
        if rc == 0 {
            // macOS does not wake a *blocking* waitpid on a continue, so poll.
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        if rc == -1 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            std::process::exit(125);
        }
        if libc::WIFSTOPPED(st) {
            say("STOPPED");
        } else if libc::WIFCONTINUED(st) {
            say("CONTINUED");
        } else if libc::WIFEXITED(st) {
            std::process::exit(libc::WEXITSTATUS(st));
        } else if libc::WIFSIGNALED(st) {
            // Only under a test's forced-kill teardown; 128 + signal mirrors
            // csm's own exit convention.
            std::process::exit(128 + libc::WTERMSIG(st));
        }
    }
}

#[cfg(unix)]
fn say(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(not(unix))]
fn main() {}
