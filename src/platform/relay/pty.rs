//! Unix pty relay: the POSIX half of [`super`]. See the parent module doc
//! for the design; this file holds everything that touches a pty, termios or
//! a POSIX signal.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{PtyMaster, Winsize, grantpt, posix_openpt, unlockpt};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{getpgrp, isatty, pipe, read as nix_read, tcgetpgrp, write as nix_write};

use super::classify::Classifier;
use super::leader::{LeaderCommand, LeaderReport};
use super::{NoopObserver, RelayIo, RelayObserver, should_activate};
use crate::config::IdleCompactMode;
use crate::platform::launcher::{ChildEnv, ChildHandle, Launcher};
use crate::platform::posix::PosixLauncher;

/// What [`RelayIo`] writes claude's input to: the inner pty's master.
pub(super) type Master = PtyMaster;

/// Write all of `bytes` to the inner pty (claude's input).
pub(super) fn write_master(master: &Master, bytes: &[u8]) -> io::Result<()> {
    write_all(master.as_fd(), bytes)
}

/// Write all of `bytes` to the outer terminal.
pub(super) fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    write_all(io::stdout().as_fd(), bytes)
}


/// I/O shell around [`should_activate`]: reads the real stdin/stdout tty
/// status, csm's own foreground-pgrp status on stdin, and `CSM_RELAY`.
pub(crate) fn platform_should_activate(mode: IdleCompactMode) -> bool {
    let stdin_is_tty = isatty(io::stdin().as_fd()).unwrap_or(false);
    let stdout_is_tty = isatty(io::stdout().as_fd()).unwrap_or(false);
    let is_foreground = stdin_is_tty
        && tcgetpgrp(io::stdin().as_fd())
            .map(|fg| fg == getpgrp())
            .unwrap_or(false);
    let csm_relay = std::env::var("CSM_RELAY").ok();
    should_activate(
        mode,
        stdin_is_tty,
        stdout_is_tty,
        is_foreground,
        csm_relay.as_deref(),
    )
}

// ─── raw-mode guard (process-global: one outer terminal, restore anywhere) ──

static ORIGINAL_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);
static PANIC_HOOK: OnceLock<()> = OnceLock::new();

fn install_panic_hook() {
    PANIC_HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_cooked();
            prev(info);
        }));
    });
}

/// Capture the outer terminal's current settings, install the panic-time
/// restore hook (once, process-wide) and switch stdin to raw mode. `Drop`
/// restores the captured settings — this is the "restore on every exit path"
/// guard; [`enter_raw`]/[`restore_cooked`] below reuse the same captured
/// value for the suspend/resume cycle without needing a second guard.
struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        install_panic_hook();
        let original = tcgetattr(io::stdin()).map_err(io::Error::from)?;
        *ORIGINAL_TERMIOS.lock().unwrap() = Some(original.clone());
        let mut raw = original;
        cfmakeraw(&mut raw);
        tcsetattr(io::stdin(), SetArg::TCSANOW, &raw).map_err(io::Error::from)?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore_cooked();
        *ORIGINAL_TERMIOS.lock().unwrap() = None;
    }
}

/// Re-enter raw mode using the settings [`RawGuard::enter`] captured
/// (used after a suspend/resume cycle). A no-op if no guard is active.
fn enter_raw() {
    let original = ORIGINAL_TERMIOS.lock().unwrap().clone();
    if let Some(original) = original {
        let mut raw = original;
        cfmakeraw(&mut raw);
        let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &raw);
    }
}

/// Put the outer terminal back in raw mode after an observer swallowed a
/// panic: the relay's panic hook restores cooked mode for a fatal panic, which
/// would leave a session that carries on with echo and line editing on.
pub(crate) fn reassert_raw() {
    enter_raw();
}

/// Restore the outer terminal to the settings captured by [`RawGuard::enter`]
/// (used before a self-suspend, on panic, and by `RawGuard`'s own `Drop`). A
/// no-op if no guard is active.
fn restore_cooked() {
    let original = ORIGINAL_TERMIOS.lock().unwrap().clone();
    if let Some(original) = original {
        let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &original);
    }
}

// ─── raw ioctls with no nix wrapper ──────────────────────────────────────────

pub(super) fn set_controlling_tty(fd: &impl AsFd) -> io::Result<()> {
    let raw = fd.as_fd().as_raw_fd();
    let rc = unsafe { libc::ioctl(raw, libc::TIOCSCTTY as _, 0) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn get_winsize(fd: impl AsFd) -> io::Result<Winsize> {
    let raw = fd.as_fd().as_raw_fd();
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(raw, libc::TIOCGWINSZ as _, &mut ws as *mut Winsize) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ws)
    }
}

fn set_winsize(fd: impl AsFd, ws: &Winsize) -> io::Result<()> {
    let raw = fd.as_fd().as_raw_fd();
    let rc = unsafe { libc::ioctl(raw, libc::TIOCSWINSZ as _, ws as *const Winsize) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_all(fd: BorrowedFd<'_>, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match nix_write(fd, buf) {
            Ok(0) => return Err(io::Error::other("write returned 0 bytes")),
            Ok(n) => buf = &buf[n..],
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::from(e)),
        }
    }
    Ok(())
}

// ─── worker threads ───────────────────────────────────────────────────────────

/// Resync the master's window size from the outer terminal's current size.
/// Returns `Ok(true)` if the size actually changed.
fn sync_size(io: &RelayIo) -> io::Result<bool> {
    let ws = get_winsize(io::stdin())?;
    set_winsize(io.master.as_fd(), &ws)?;
    let (rows, cols) = (ws.ws_row, ws.ws_col);
    let changed = io.size() != (rows, cols);
    io.set_size(rows, cols);
    Ok(changed)
}

fn shutdown_requested(fds: &[PollFd<'_>], idx: usize) -> bool {
    fds[idx].revents().map(|r| !r.is_empty()).unwrap_or(true)
}

/// The outer terminal went away (EOF, EIO or POLLHUP on stdin: raw mode makes
/// a real Ctrl-D an ordinary byte). Routed through the same self-pipe as a
/// real SIGHUP, so the signal thread forwards SIGHUP to claude's process
/// group via the leader, exactly as the terminal driver would for a direct
/// launch. Some kernels signal only the session leader on a master close, so
/// csm cannot rely on receiving the signal itself.
fn outer_hangup() {
    relay_signal_handler(Signal::SIGHUP as std::ffi::c_int);
}

fn input_thread(io: Arc<RelayIo>, shutdown_r: Arc<OwnedFd>) {
    let mut classifier = Classifier::new();
    let stdin = io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [
            PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
            PollFd::new(shutdown_r.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => {
                outer_hangup();
                break;
            }
        }
        if shutdown_requested(&fds, 1) {
            break;
        }
        let ready = fds[0].revents().unwrap_or(PollFlags::empty());
        if ready.contains(PollFlags::POLLIN) {
            match nix_read(stdin.as_fd(), &mut buf) {
                Ok(0) => {
                    outer_hangup();
                    break;
                }
                Ok(n) => {
                    let bytes = &buf[..n];
                    if classifier.feed(bytes) {
                        io.note_keystroke();
                    }
                    if io.is_held() {
                        io.buffer_held(bytes);
                    } else {
                        let _ = io.write_master(bytes);
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => {
                    outer_hangup();
                    break;
                }
            }
        } else if ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            outer_hangup();
            break;
        }
    }
}

fn output_thread(io: Arc<RelayIo>, observer: Arc<dyn RelayObserver>, shutdown_r: Arc<OwnedFd>) {
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [
            PollFd::new(io.master.as_fd(), PollFlags::POLLIN),
            PollFd::new(shutdown_r.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        if shutdown_requested(&fds, 1) {
            break;
        }
        let ready = fds[0].revents().unwrap_or(PollFlags::empty());
        if ready.contains(PollFlags::POLLIN) {
            match nix_read(io.master.as_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    io.note_output();
                    let _ = io.relay_output(&buf[..n]);
                    observer.on_output(&buf[..n]);
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        } else if ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
    }
}

/// Signals the relay reacts to. Handled with the self-pipe pattern: the
/// handler only writes the signal number to a non-blocking pipe (async-signal
/// safe) and the signal thread does the real work in normal thread context.
///
/// Not the "blocked mask + sigwait" alternative: on macOS a blocked SIGCONT
/// resumes the threads of a stopped process but leaves the process-level
/// stop state set (ps keeps showing `T`, the parent's waitpid never sees the
/// continue), which breaks job control for csm's own shell. Handlers keep the
/// kernel's stop/continue bookkeeping untouched. SIGTSTP is deliberately not
/// handled, so csm's self-suspend keeps the default action.
const RELAY_SIGNALS: [Signal; 5] = [
    Signal::SIGWINCH,
    Signal::SIGCONT,
    Signal::SIGTERM,
    Signal::SIGHUP,
    Signal::SIGINT,
];

static SIGNAL_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn relay_signal_handler(sig: std::ffi::c_int) {
    let fd = SIGNAL_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        // errno must survive the handler; a full pipe just drops the byte
        // (the same signal is already pending there).
        let saved = nix::errno::Errno::last_raw();
        let byte = sig as u8;
        unsafe {
            libc::write(fd, &byte as *const u8 as *const std::ffi::c_void, 1);
        }
        nix::errno::Errno::set_raw(saved);
    }
}

/// Installs the relay's signal handlers and the pipe they write to; `Drop`
/// restores the previous dispositions. Created before the leader is spawned,
/// so a signal arriving during startup is queued instead of lost or fatal.
struct SignalGuard {
    read: OwnedFd,
    write: OwnedFd,
    previous: Vec<(Signal, SigAction)>,
}

fn set_nonblock_cloexec(fd: &OwnedFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io::Error::from)?;
    Ok(())
}

impl SignalGuard {
    fn install() -> io::Result<Self> {
        let (read, write) = pipe().map_err(io::Error::from)?;
        set_nonblock_cloexec(&read)?;
        set_nonblock_cloexec(&write)?;
        SIGNAL_PIPE_W.store(write.as_raw_fd(), Ordering::SeqCst);
        let mut guard = SignalGuard {
            read,
            write,
            previous: Vec::new(),
        };
        let act = SigAction::new(
            SigHandler::Handler(relay_signal_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        for sig in RELAY_SIGNALS {
            let prev = unsafe { sigaction(sig, &act) }.map_err(io::Error::from)?;
            guard.previous.push((sig, prev));
        }
        Ok(guard)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, prev) in &self.previous {
            let _ = unsafe { sigaction(*sig, prev) };
        }
        SIGNAL_PIPE_W.store(-1, Ordering::SeqCst);
        let _ = &self.write;
    }
}

fn signal_thread(
    io: Arc<RelayIo>,
    observer: Arc<dyn RelayObserver>,
    mut leader_stdin: ChildStdin,
    sig_r: Arc<OwnedFd>,
    shutdown_r: Arc<OwnedFd>,
) {
    let mut buf = [0u8; 64];
    'outer: loop {
        let mut fds = [
            PollFd::new(sig_r.as_fd(), PollFlags::POLLIN),
            PollFd::new(shutdown_r.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        if shutdown_requested(&fds, 1) {
            break;
        }
        let n = match nix_read(sig_r.as_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        };
        for &byte in &buf[..n] {
            let Ok(sig) = Signal::try_from(byte as i32) else {
                continue;
            };
            if !handle_signal(sig, &io, &observer, &mut leader_stdin) {
                break 'outer;
            }
        }
    }
}

/// Returns false when the leader pipe is gone and there is nothing left to do.
fn handle_signal(
    sig: Signal,
    io: &RelayIo,
    observer: &Arc<dyn RelayObserver>,
    leader_stdin: &mut ChildStdin,
) -> bool {
    let cmd = match sig {
        Signal::SIGWINCH => {
            if let Ok(true) = sync_size(io) {
                let (rows, cols) = io.size();
                observer.on_resize(rows, cols);
            }
            None
        }
        Signal::SIGCONT => {
            enter_raw();
            if let Ok(true) = sync_size(io) {
                let (rows, cols) = io.size();
                observer.on_resize(rows, cols);
            }
            Some(LeaderCommand::Cont)
        }
        Signal::SIGTERM | Signal::SIGHUP | Signal::SIGINT => {
            Some(LeaderCommand::Signal(sig as i32))
        }
        _ => None,
    };
    match cmd {
        Some(cmd) => writeln!(leader_stdin, "{}", cmd.format())
            .and_then(|_| leader_stdin.flush())
            .is_ok(),
        None => true,
    }
}

/// Reads the leader's remaining report lines (after the synchronous `pid`
/// line already consumed before this thread was spawned). `stopped` drives
/// the self-suspend dance in place; `exit`/`signal` is sent once on `tx` and
/// ends the thread.
fn leader_control_thread(
    mut reader: BufReader<std::process::ChildStdout>,
    tx: mpsc::Sender<LeaderReport>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => match LeaderReport::parse(&line) {
                Some(LeaderReport::Stopped) => {
                    restore_cooked();
                    let _ = nix::sys::signal::raise(Signal::SIGTSTP);
                    // Execution resumes here once SIGCONT wakes the whole
                    // process; the signal thread (unaffected by this
                    // thread's own state) is what re-enters raw mode and
                    // tells the leader `cont`.
                }
                Some(r @ (LeaderReport::Exit(_) | LeaderReport::Signal(_))) => {
                    let _ = tx.send(r);
                    break;
                }
                Some(LeaderReport::Pid(_)) | None => {}
            },
            Err(_) => break,
        }
    }
}

// ─── RelayLauncher ─────────────────────────────────────────────────────────

enum Failure {
    /// Before claude was confirmed alive: fall back to the direct launcher
    /// for this hop.
    Setup(String),
    /// After claude was already running: propagate as-is.
    Fatal(io::Error),
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Fatal(e)
    }
}

impl From<nix::errno::Errno> for Failure {
    fn from(e: nix::errno::Errno) -> Self {
        Failure::Fatal(io::Error::from(e))
    }
}

/// Unix pty relay launcher. See the module doc for the full design.
pub struct RelayLauncher {
    fallback: PosixLauncher,
    observer: Arc<dyn RelayObserver>,
}

impl RelayLauncher {
    pub fn new() -> Self {
        RelayLauncher {
            fallback: PosixLauncher,
            observer: Arc::new(NoopObserver),
        }
    }

    pub fn with_observer(observer: Arc<dyn RelayObserver>) -> Self {
        RelayLauncher {
            fallback: PosixLauncher,
            observer,
        }
    }
}

impl Default for RelayLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher for RelayLauncher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &ChildEnv,
        on_spawn: &mut dyn FnMut(),
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        match self.try_run_foreground(sid, cli, env, on_spawn) {
            Ok(result) => Ok(result),
            Err(Failure::Setup(msg)) => {
                let _ = crate::hook::notify::append_log(
                    sid,
                    &format!("pty relay unavailable ({msg}); falling back to direct launcher"),
                );
                self.fallback.run_foreground(sid, cli, env, on_spawn)
            }
            Err(Failure::Fatal(e)) => Err(e),
        }
    }
}

impl RelayLauncher {
    fn try_run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &ChildEnv,
        on_spawn: &mut dyn FnMut(),
    ) -> Result<(ExitStatus, ChildHandle), Failure> {
        let raw_guard = RawGuard::enter().map_err(|e| Failure::Setup(format!("raw mode: {e}")))?;

        // O_CLOEXEC: this master fd is this process's own private handle on
        // the inner pty. Nothing the leader (or, through it, claude) spawns
        // needs it, and without CLOEXEC it would otherwise silently survive
        // the leader's exec (and cascade into claude's own fd table) since
        // `std::process::Command` inherits any fd that doesn't have
        // close-on-exec set — a stray reference on it defeats the purpose of
        // "the parent never keeps a slave fd open" below for the master
        // side, and (found via the outer-terminal-hangup integration test)
        // an equivalent leak on the *outer* pty's master is exactly what
        // silently prevents a real hangup from ever being detected: the
        // kernel only generates it once every open reference to the master
        // is gone.
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC)
            .map_err(|e| Failure::Setup(format!("posix_openpt: {e}")))?;
        grantpt(&master).map_err(|e| Failure::Setup(format!("grantpt: {e}")))?;
        unlockpt(&master).map_err(|e| Failure::Setup(format!("unlockpt: {e}")))?;
        let slave_path = unsafe { nix::pty::ptsname(&master) }
            .map_err(|e| Failure::Setup(format!("ptsname: {e}")))?;

        // Copy the outer terminal's (pre-raw) settings onto the slave, then
        // close this fd — the parent (relay) never keeps a slave fd open;
        // the leader and claude open their own. Read from `ORIGINAL_TERMIOS`
        // (what `RawGuard::enter` captured above), NOT a fresh `tcgetattr`
        // on stdin here — by this point `RawGuard::enter` has already put
        // the outer terminal into raw mode, so a fresh read would copy the
        // *raw* settings (ISIG off) onto the inner pty instead of the
        // cooked ones, silently breaking the kernel's own VSUSP handling
        // there: a Ctrl-Z byte relayed onto an ISIG-off inner pty is never
        // turned into a real SIGTSTP for claude's process group, so the
        // suspend/resume protocol below (`leader_control_thread`'s
        // `Stopped` handling) would never even begin.
        let outer_termios = ORIGINAL_TERMIOS
            .lock()
            .unwrap()
            .clone()
            .expect("RawGuard::enter just set this");
        {
            let slave_fd = nix::fcntl::open(
                slave_path.as_str(),
                OFlag::O_RDWR | OFlag::O_NOCTTY,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(|e| Failure::Setup(format!("open slave: {e}")))?;
            let _ = tcsetattr(&slave_fd, SetArg::TCSANOW, &outer_termios);
        }

        let initial_ws =
            get_winsize(io::stdin()).map_err(|e| Failure::Setup(format!("TIOCGWINSZ: {e}")))?;
        set_winsize(master.as_fd(), &initial_ws)
            .map_err(|e| Failure::Setup(format!("TIOCSWINSZ: {e}")))?;

        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(|e| Failure::Setup(format!("current_exe: {e}")))?;
        let launch = crate::config::launch_command_for_spawn()
            .map_err(|e| Failure::Setup(format!("launch command: {e}")))?;

        let mut cmd = Command::new(&exe);
        cmd.arg("__pty-leader").arg(&slave_path).arg("--");
        cmd.args(&launch);
        cmd.args(cli);
        let mut leader_env = env.clone();
        leader_env.set.insert(
            OsString::from(crate::idle_compact::SUPERVISOR_PID_ENV),
            OsString::from(std::process::id().to_string()),
        );
        leader_env.apply(&mut cmd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let signals =
            SignalGuard::install().map_err(|e| Failure::Setup(format!("signal setup: {e}")))?;
        let mut leader: Child = cmd
            .spawn()
            .map_err(|e| Failure::Setup(format!("spawn leader: {e}")))?;
        let leader_stdin = leader.stdin.take().expect("piped stdin");
        let leader_stdout = leader.stdout.take().expect("piped stdout");
        let leader_stderr = leader.stderr.take().expect("piped stderr");

        let mut reader = BufReader::new(leader_stdout);
        let mut first_line = String::new();
        let got = reader.read_line(&mut first_line);
        let pid = match got.ok().and_then(|_| LeaderReport::parse(&first_line)) {
            Some(LeaderReport::Pid(p)) => p,
            _ => {
                // The leader never got claude running. Reap it and pass its
                // own diagnostic (its stderr) into the fallback log line.
                let _ = leader.kill();
                let _ = leader.wait();
                let mut diag = String::new();
                let _ = io::Read::read_to_string(&mut BufReader::new(leader_stderr), &mut diag);
                return Err(Failure::Setup(format!(
                    "leader did not report a pid: {}",
                    diag.trim()
                )));
            }
        };
        let born = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = crate::platform::pid::write_pid_file(&crate::paths::pid_file(sid), pid, born);
        on_spawn();

        // From here on claude is real and running: no more setup fallback.
        let io = Arc::new(RelayIo::new(master, (initial_ws.ws_row, initial_ws.ws_col)));
        self.observer.on_start(Arc::clone(&io), pid, env);

        let (shutdown_r, shutdown_w) = pipe()?;
        let shutdown_r = Arc::new(shutdown_r);
        let sig_r = Arc::new(signals.read.try_clone()?);

        let log_sid = sid.to_string();
        thread::spawn(move || {
            for line in BufReader::new(leader_stderr).lines().map_while(Result::ok) {
                let _ = crate::hook::notify::append_log(&log_sid, &format!("pty-leader: {line}"));
            }
        });

        let (tx, rx) = mpsc::channel::<LeaderReport>();
        let leader_ctl = thread::spawn(move || leader_control_thread(reader, tx));
        let input = thread::spawn({
            let io = Arc::clone(&io);
            let shutdown_r = Arc::clone(&shutdown_r);
            move || input_thread(io, shutdown_r)
        });
        let output = thread::spawn({
            let io = Arc::clone(&io);
            let shutdown_r = Arc::clone(&shutdown_r);
            let observer = Arc::clone(&self.observer);
            move || output_thread(io, observer, shutdown_r)
        });
        let signal = thread::spawn({
            let io = Arc::clone(&io);
            let shutdown_r = Arc::clone(&shutdown_r);
            let observer = Arc::clone(&self.observer);
            move || signal_thread(io, observer, leader_stdin, sig_r, shutdown_r)
        });

        // Block for the final report. A closed channel (leader died without
        // ever reporting exit/signal) is treated as an abnormal death.
        let final_report = rx.recv().unwrap_or(LeaderReport::Signal(libc::SIGKILL));

        drop(shutdown_w);

        let _ = input.join();
        let _ = output.join();
        let _ = signal.join();
        let _ = leader_ctl.join();
        let _ = leader.wait();
        drop(signals);
        drop(raw_guard);
        self.observer.on_exit();

        let status = match final_report {
            LeaderReport::Exit(code) => ExitStatus::from_raw((code & 0xff) << 8),
            LeaderReport::Signal(sig) => ExitStatus::from_raw(sig),
            LeaderReport::Pid(_) | LeaderReport::Stopped => {
                unreachable!("only Exit/Signal are sent on tx")
            }
        };
        Ok((status, ChildHandle { pid, born }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hold_input`/`inject` ordering, exercised directly against `RelayIo`
    /// (no leader/claude process needed): held bytes are buffered and only
    /// reach the master once the outermost hold is dropped, in the order
    /// they were fed; `inject` bypasses the hold entirely and lands
    /// immediately, so an inject issued *during* a hold is visible on the
    /// master *before* the held bytes the hold later flushes.
    #[test]
    fn hold_input_buffers_in_order_inject_bypasses_the_hold() {
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).expect("posix_openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let slave_path = unsafe { nix::pty::ptsname(&master) }.expect("ptsname");
        let slave = nix::fcntl::open(
            slave_path.as_str(),
            OFlag::O_RDWR | OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        )
        .expect("open slave");
        // A freshly opened pty defaults to canonical mode: without this the
        // kernel would never release the test's (newline-free) payload to
        // the blocking `read` below.
        let mut slave_termios = tcgetattr(&slave).expect("tcgetattr slave");
        cfmakeraw(&mut slave_termios);
        tcsetattr(&slave, SetArg::TCSANOW, &slave_termios).expect("tcsetattr slave raw");

        let io = RelayIo::new(master, (24, 80));
        let hold = io.hold_input();
        io.buffer_held(b"first ");
        io.inject(b"INJECTED ").expect("inject");
        io.buffer_held(b"second");
        drop(hold);

        let want = b"INJECTED first second";
        let mut got = Vec::new();
        let mut buf = [0u8; 128];
        for _ in 0..200 {
            if got.len() >= want.len() {
                break;
            }
            match nix_read(&slave, &mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => {
                    thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("read slave: {e}"),
            }
        }
        assert_eq!(got, want, "got {:?}", String::from_utf8_lossy(&got));
    }
}
