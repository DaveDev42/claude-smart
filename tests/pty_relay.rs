//! Integration tests for the Unix pty relay launcher
//! ([`crate::platform::relay`] — see `src/platform/relay/mod.rs`'s module
//! doc for the design these tests exercise).
//!
//! Every test drives the *built* `csm` binary (via `CARGO_BIN_EXE_csm`) under
//! a real pty, exactly as a real terminal would. It is a fully black-box
//! suite: nothing here calls into `claude-smart`'s own code, only spawns
//! processes and reads/writes fds, so it also stands as living documentation
//! of the observable contract.
//!
//! Three test-only binaries (see their own doc comments) make this possible:
//!   - `pty_harness <slave-path> <program> [args...]` — becomes the session
//!     leader of the outer pty (so csm gets a real controlling terminal with
//!     itself as the foreground process group — a `#[test]` function cannot
//!     safely call `setsid()` itself, see `tests/bin/pty_harness.rs`), then
//!     hands the terminal to `program` as a real, non-orphaned job-control
//!     child (its own process group, this harness staying alive as "the
//!     shell") and reports its real pid on its own stdout as `PID <n>\n`
//!     (`Session::pid()` — `program` is a different OS process from this
//!     harness, unlike the pre-2026-09 exec-in-place design; see the
//!     harness's own doc comment for why an orphaned process group would
//!     silently break `SIGTSTP`-based suspend).
//!   - `fake_claude_relay` — stands in for `claude` via `CLAUDE_SMART_CLAUDE_BIN`,
//!     the same override csm's own launch-command resolver reads. Never runs
//!     the real `claude`.
//!   - `csm` itself, run only against a scratch `HOME` (a fresh `tempfile::TempDir`
//!     per test): every spawn sets `HOME` there and never touches the real
//!     home directory, `~/.claude*`, `~/.config`, `~/.local` or `~/Library`.
//!
//! `idleCompact` is fixed at `"dry-run"` in every scratch home's
//! `.config/claude-smart/config.json`, matching the requirement that these
//! tests exercise the real activation predicate (`mode != off`) without
//! idle-compact's own tick/typing behaviour (not part of this launcher) ever
//! engaging.

#![cfg(unix)]

use std::fs;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{PtyMaster, grantpt, posix_openpt, unlockpt};
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::termios::{Termios, tcgetattr};
use nix::unistd::Pid;

// ─── env scrub (hard-rule contract: tests never leak these into a spawned csm) ─

/// Every `CSM_*`/`ORCA_*`/`ZELLIJ*`/`WEZTERM*` var plus `CLAUDE_CONFIG_DIR`
/// this crate reads anywhere, enumerated by hand (not derived) so a stray
/// value on the machine running these tests can never leak into a spawned
/// csm and change its behaviour. `CSM_RELAY` and `CSM_SUPERVISOR_PID` are
/// scrubbed here too — each test sets them explicitly when it needs a
/// specific value.
const SCRUBBED_VARS: &[&str] = &[
    "CSM_EMBEDDED",
    "CSM_HOST_REPLACE",
    "CSM_OAUTH_TOKEN_URL",
    "CSM_ORCA",
    "CSM_RELAY",
    "CSM_STATUSLINE_NO_CAPTURE",
    "CSM_SUPERVISOR_PID",
    "CSM_TEST_ENV_SLEEPER",
    "CSM_TEST_ENVVAR_ALIAS",
    "CSM_TEST_ENVVAR_PRIMARY",
    "CSM_USAGE_API_BASE",
    "CSM_USAGE_CMD_TIMEOUT",
    "CSM_USAGE_CMD",
    "CSM_USAGE_MAX_AGE_SECS",
    "CSM_USAGE_PROFILE_TTL",
    "CSM_USAGE_RATE_LIMIT_COOLDOWN",
    "CSM_USAGE_TTL_SECS",
    "ORCA_AGENT_HOOK_ENDPOINT",
    "ORCA_AGENT_HOOK_ENV",
    "ORCA_AGENT_HOOK_PORT",
    "ORCA_AGENT_HOOK_TOKEN",
    "ORCA_AGENT_HOOK_VERSION",
    "ORCA_AGENT_LAUNCH_TOKEN",
    "ORCA_AGENT_SESSION_SPAWN_TOKEN",
    "ORCA_APP_VERSION",
    "ORCA_PANE_KEY",
    "ORCA_TERMINAL_HANDLE",
    "ORCA_USER_DATA_PATH",
    "WEZTERM_PANE",
    "ZELLIJ_PANE_ID",
    "ZELLIJ_SESSION_NAME",
    "CLAUDE_CONFIG_DIR",
    // Not part of the enumerated contract list, but a real XDG override on
    // the host running these tests would move the pid-file/state dir out
    // from under the scratch HOME the pid-file-matching test reads back.
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
];

// ─── scratch HOME ─────────────────────────────────────────────────────────────

/// A fresh `HOME` for one test: `.config/claude-smart/config.json` fixed at
/// `idleCompact: dry-run`. Dropped (and deleted) at the end of the test:
/// never the real home directory.
struct TestHome {
    dir: tempfile::TempDir,
}

impl TestHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_dir = dir.path().join(".config").join("claude-smart");
        fs::create_dir_all(&cfg_dir).expect("mkdir config dir");
        fs::write(cfg_dir.join("config.json"), br#"{"idleCompact":"dry-run"}"#)
            .expect("write config.json");
        TestHome { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `<home>/.local/state/csm/<sid>.pid` — matches `paths::pid_file` with
    /// no `XDG_STATE_HOME` override (scrubbed above).
    fn pid_file(&self, sid: &str) -> PathBuf {
        self.path()
            .join(".local")
            .join("state")
            .join("csm")
            .join(format!("{sid}.pid"))
    }
}

/// The base environment every spawned process gets: the test runner's own
/// environment with every var in [`SCRUBBED_VARS`] removed, `HOME` pointed
/// at the scratch home, and `CLAUDE_SMART_CLAUDE_BIN` pointed at the fake
/// claude binary so nothing here ever runs the real `claude`.
fn base_env(home: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !SCRUBBED_VARS.contains(&k.as_str()))
        .collect();
    env.retain(|(k, _)| k != "HOME");
    env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    env.push((
        "CLAUDE_SMART_CLAUDE_BIN".to_string(),
        std::env::var("CARGO_BIN_EXE_fake_claude_relay").expect("CARGO_BIN_EXE_fake_claude_relay"),
    ));
    env
}

fn apply_env(cmd: &mut Command, home: &Path, extra: &[(&str, &str)]) {
    cmd.env_clear();
    for (k, v) in base_env(home) {
        cmd.env(k, v);
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
}

// ─── pty helpers (same posix_openpt/grantpt/unlockpt recipe as the relay itself) ─

fn open_pty() -> (PtyMaster, String) {
    // O_CLOEXEC: matches the relay's own posix_openpt call (see the comment
    // there). Without it, `spawn_via_harness`'s master fd would silently
    // survive into pty_harness's exec of csm and everything csm itself goes
    // on to spawn — a stray copy anywhere downstream is enough to keep this
    // master's open-reference count off zero, which is exactly what defeats
    // `outer_hangup_sends_sighup_to_claude`'s "close every reference to the
    // master" simulated hangup: the kernel only ever generates it once the
    // last reference is gone.
    let master =
        posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).expect("posix_openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    // `ptsname` returns a pointer into a process-wide static buffer, so two
    // tests opening a pty at the same moment can be handed each other's
    // slave path. Linux has the reentrant `ptsname_r`; elsewhere the call is
    // serialised.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let slave_path = nix::pty::ptsname_r(&master).expect("ptsname_r");
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let slave_path = {
        static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = PTSNAME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { nix::pty::ptsname(&master) }.expect("ptsname")
    };
    (master, slave_path)
}

/// Like [`open_pty`], but for a pty that will never have its slave opened by
/// anyone else (a throwaway termios baseline, never handed to a subprocess):
/// opens and immediately drops the slave once, to seed the pty's termios
/// state, since on this platform a freshly allocated pty's master fails
/// `tcgetattr` with `ENOTTY` until the slave has been opened at least once
/// (the relay itself never hits this in practice, since csm always opens the
/// slave once too, right after `unlockpt`, to copy the outer termios onto it
/// — see `try_run_foreground`). Do NOT use this for a pty that a subprocess
/// (`pty_harness`) will go on to open itself: dropping the slave here, even
/// briefly, before that process gets its own open in means the slave's
/// open-reference count touches zero, which this platform's master then
/// reports as a permanent (sticky) hangup on the *next* open — breaking the
/// whole session before it starts, not just this probe.
fn open_pty_seeded() -> (PtyMaster, String) {
    let (master, slave_path) = open_pty();
    let slave = nix::fcntl::open(
        slave_path.as_str(),
        OFlag::O_RDWR | OFlag::O_NOCTTY,
        nix::sys::stat::Mode::empty(),
    )
    .expect("open slave once to seed termios");
    drop(slave);
    (master, slave_path)
}

/// Read whatever is available on `fd` for up to `timeout`, stopping early
/// once a quiet gap follows some bytes (so a test does not have to pay the
/// full timeout for output that already arrived). Works for any readable fd
/// (the outer pty's master, or a plain pipe in the non-tty-stdin test).
fn read_available(fd: BorrowedFd<'_>, timeout: Duration) -> Vec<u8> {
    let mut buf = Vec::new();
    let start = Instant::now();
    let mut tmp = [0u8; 4096];
    loop {
        if start.elapsed() >= timeout {
            break;
        }
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(200u16)) {
            Ok(0) => {
                if !buf.is_empty() {
                    break;
                }
                continue;
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        let revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if revents.contains(PollFlags::POLLIN) {
            match nix::unistd::read(fd, &mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        } else if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
    }
    buf
}

/// Read until `buf` contains `marker` or `timeout` elapses (unlike
/// [`read_available`], never stops early on a quiet gap — csm's own startup,
/// before it ever touches the inner pty, can take a few seconds).
fn wait_for(fd: BorrowedFd<'_>, marker: &[u8], timeout: Duration) -> Vec<u8> {
    let mut buf = Vec::new();
    let start = Instant::now();
    let mut tmp = [0u8; 4096];
    while start.elapsed() < timeout {
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(200u16)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        let revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if revents.contains(PollFlags::POLLIN) {
            match nix::unistd::read(fd, &mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if contains(&buf, marker) {
                        return buf;
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        } else if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
    }
    buf
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn write_all_master(master: &PtyMaster, bytes: &[u8]) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match nix::unistd::write(master.as_fd(), remaining) {
            Ok(n) => remaining = &remaining[n..],
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => panic!("write master: {e}"),
        }
    }
}

/// Split accumulated pty output into trimmed lines (`\r\n` or `\n`
/// terminated; a pty's OPOST/ONLCR processing means real output is `\r\n`,
/// but tests match on content, not line-ending style).
fn lines_of(buf: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(buf)
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

// ─── spawn helpers ─────────────────────────────────────────────────────────────

struct Session {
    /// The spawned `pty_harness` process itself — csm's "shell" (see its own
    /// doc comment). Its own exit status mirrors csm's (`wait_exit` below),
    /// but its pid is *not* csm's own; use `pid()` for that.
    child: Child,
    master: PtyMaster,
    /// csm's real pid, read from `pty_harness`'s `PID <n>\n` line once, right
    /// after spawn (see `spawn_via_harness`).
    csm_pid: u32,
    /// Job-control lines (`STOPPED` / `CONTINUED`) the harness reports as its
    /// waitpid(WUNTRACED | WCONTINUED) sees csm's state change.
    events: std::sync::mpsc::Receiver<String>,
}

impl Session {
    /// Wait for the harness to report `event` (`STOPPED` or `CONTINUED`).
    fn wait_event(&self, event: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(l) if l == event => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    /// csm's own real pid — not `pty_harness`'s (see the `child` field doc).
    fn pid(&self) -> u32 {
        self.csm_pid
    }

    /// Poll `child` (the harness) for exit for up to `timeout`. `Some(code)`
    /// once it has exited (relies on csm's own `exit_with`, which always
    /// calls `std::process::exit` with either the launched program's own
    /// exit code, or `128 + signal` for a signal death, and on
    /// `pty_harness`'s own exit mirroring csm's via the same convention —
    /// see its doc comment — so a plain exit code is enough to observe both
    /// cases from outside).
    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill_if_alive(&mut self) {
        // csm is `pty_harness`'s child, not the process this test spawned
        // directly (see the `child` field doc) — killing only `self.child`
        // would leave csm (and, through it, the leader/claude subtree, which
        // notices csm's stdin pipe close and cascades a SIGHUP down — see
        // `leader.rs`) running, re-parented and orphaned. Kill csm directly
        // first; `pty_harness` then notices (its blocking `wait_for` in
        // `tests/bin/pty_harness.rs` returns) and exits on its own, but it is
        // also force-killed below for a deterministic, bounded teardown.
        let _ = kill(Pid::from_raw(self.csm_pid as i32), Signal::SIGKILL);
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_if_alive();
    }
}

/// Spawn `csm <csm_args>` through `pty_harness`, giving it a real
/// controlling terminal (raw-mode-capable, session-leader-owned) on a fresh
/// outer pty — the only way relay mode's activation predicate can ever see
/// `is_foreground == true`. Returns the session plus the outer pty's
/// `master` the test drives.
fn spawn_via_harness(home: &TestHome, csm_args: &[&str], extra_env: &[(&str, &str)]) -> Session {
    let (master, slave_path) = open_pty();
    let harness = std::env::var("CARGO_BIN_EXE_pty_harness").expect("CARGO_BIN_EXE_pty_harness");
    let csm = std::env::var("CARGO_BIN_EXE_csm").expect("CARGO_BIN_EXE_csm");

    let mut cmd = Command::new(&harness);
    cmd.arg(&slave_path).arg(&csm).args(csm_args);
    apply_env(&mut cmd, home.path(), extra_env);
    // stdout piped (not null): `pty_harness` reports csm's real pid as
    // `PID <n>\n` on it, before touching the pty at all — see its own doc
    // comment and `Session`'s `child`/`csm_pid` field docs.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn pty_harness");

    use std::io::BufRead as _;
    let mut reader = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read PID line from pty_harness");
    let csm_pid: u32 = line
        .trim()
        .strip_prefix("PID ")
        .expect("PID line prefix")
        .parse()
        .expect("PID line parses");
    let (tx, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut l = String::new();
        while reader.read_line(&mut l).map(|n| n > 0).unwrap_or(false) {
            let _ = tx.send(l.trim().to_string());
            l.clear();
        }
    });

    Session {
        child,
        master,
        csm_pid,
        events,
    }
}

/// Spawn `csm <csm_args>` directly, stdio piped (never a tty) — used only by
/// the "stdin is not a tty" activation test.
fn spawn_direct_piped(home: &TestHome, csm_args: &[&str], extra_env: &[(&str, &str)]) -> Child {
    let csm = std::env::var("CARGO_BIN_EXE_csm").expect("CARGO_BIN_EXE_csm");
    let mut cmd = Command::new(&csm);
    cmd.args(csm_args);
    apply_env(&mut cmd, home.path(), extra_env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn csm")
}

/// The three startup lines every `fake_claude_relay` prints, parsed:
/// `READY <pid>`, `ARGV <args...>`, `SUPERVISOR <value|->`.
struct Startup {
    claude_pid: u32,
    argv: String,
    supervisor: Option<String>,
}

fn read_startup(master: &PtyMaster, timeout: Duration) -> Startup {
    let mut buf = wait_for(master.as_fd(), b"SUPERVISOR", timeout);
    // The marker above only proves the word "SUPERVISOR" has arrived, not
    // necessarily its value or trailing newline — grab a bit more.
    buf.extend(read_available(master.as_fd(), Duration::from_millis(300)));
    let lines = lines_of(&buf);
    let ready = lines
        .iter()
        .find(|l| l.starts_with("READY "))
        .unwrap_or_else(|| panic!("no READY line in {lines:?}"));
    let claude_pid: u32 = ready
        .trim_start_matches("READY ")
        .trim()
        .parse()
        .expect("READY pid parses");
    let argv = lines
        .iter()
        .find(|l| l.starts_with("ARGV "))
        .map(|l| l.trim_start_matches("ARGV ").to_string())
        .unwrap_or_default();
    let supervisor = lines
        .iter()
        .find(|l| l.starts_with("SUPERVISOR "))
        .map(|l| l.trim_start_matches("SUPERVISOR ").to_string());
    let supervisor = supervisor.filter(|v| v != "-");
    Startup {
        claude_pid,
        argv,
        supervisor,
    }
}

fn session_id_from_argv(argv: &str) -> String {
    let tokens: Vec<&str> = argv.split_whitespace().collect();
    for pair in tokens.windows(2) {
        if pair[0] == "--session-id" || pair[0] == "--resume" {
            return pair[1].to_string();
        }
    }
    panic!("no --session-id/--resume in argv: {argv}");
}

fn get_termios(master: &PtyMaster) -> Termios {
    tcgetattr(master.as_fd()).expect("tcgetattr")
}

fn termios_eq(a: &Termios, b: &Termios) -> bool {
    a.input_flags == b.input_flags
        && a.output_flags == b.output_flags
        && a.control_flags == b.control_flags
        && a.local_flags == b.local_flags
        && a.control_chars == b.control_chars
}

// ─── tests ──────────────────────────────────────────────────────────────────

/// Relay mode activates when csm has a real controlling terminal: the
/// process tree shows the hidden `__pty-leader` re-exec between csm and
/// claude, and the outer terminal is in raw mode (`ICANON`/`ECHO` both off)
/// once claude is up.
#[test]
fn relay_activates_and_enters_raw_mode() {
    let home = TestHome::new();
    let before = {
        let (m, _) = open_pty_seeded();
        get_termios(&m)
    };
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    let startup = read_startup(&session.master, Duration::from_secs(10));

    assert!(
        contains(startup.argv.as_bytes(), b"--session-id")
            || contains(startup.argv.as_bytes(), b"--resume")
    );

    let now = get_termios(&session.master);
    assert!(
        !now.local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON),
        "expected raw mode (ICANON off)"
    );
    assert!(
        !now.local_flags
            .contains(nix::sys::termios::LocalFlags::ECHO),
        "expected raw mode (ECHO off)"
    );
    assert!(
        !termios_eq(&before, &now),
        "termios should differ from a freshly-opened pty once raw mode is entered"
    );

    session.kill_if_alive();
}

/// Bytes typed on the outer terminal reach claude and its response comes
/// back through the relay unchanged — no local kernel echo (raw mode), only
/// `fake_claude_relay`'s own explicit `ECHO ...` reply.
#[test]
fn bytes_relay_both_ways() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    write_all_master(&session.master, b"hello there\n");
    let out = wait_for(
        session.master.as_fd(),
        b"ECHO hello there",
        Duration::from_secs(5),
    );
    let lines = lines_of(&out);
    assert!(
        lines.iter().any(|l| l == "ECHO hello there"),
        "got {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l == "hello there"),
        "raw mode must not locally echo: {lines:?}"
    );

    session.kill_if_alive();
}

/// A `SIGWINCH` on the outer terminal resizes the *inner* pty claude sits
/// on: the relay's signal thread resyncs `TIOCSWINSZ` on the master from
/// the outer terminal's current size, and the kernel's own SIGWINCH-on-size-
/// change delivers to claude, which reports the size it now sees.
#[test]
fn resize_propagates_to_inner_pty() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    let ws = nix::pty::Winsize {
        ws_row: 41,
        ws_col: 137,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::ioctl(
            session.master.as_fd().as_raw_fd(),
            libc::TIOCSWINSZ as _,
            &ws as *const _,
        )
    };
    assert_eq!(rc, 0, "TIOCSWINSZ on outer master");
    kill(Pid::from_raw(session.pid() as i32), Signal::SIGWINCH).expect("kill SIGWINCH");

    let out = wait_for(session.master.as_fd(), b"RESIZED", Duration::from_secs(5));
    let lines = lines_of(&out);
    let resized = lines
        .iter()
        .find(|l| l.starts_with("RESIZED "))
        .unwrap_or_else(|| panic!("no RESIZED line in {lines:?}"));
    assert_eq!(resized, "RESIZED 41 137");

    session.kill_if_alive();
}

/// `claude`'s exit code propagates through the leader → relay → csm's own
/// process exit (`exit_with`'s plain-code path).
#[test]
fn exit_code_propagates() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    write_all_master(&session.master, b"EXIT 42\n");
    let _ = read_available(session.master.as_fd(), Duration::from_secs(3));
    let code = session.wait_exit(Duration::from_secs(10));
    assert_eq!(code, Some(42));
}

/// `claude` dying from a signal propagates as csm's own `128 + signal` exit
/// code (`exit_with`'s signal-death path).
#[test]
fn signal_death_propagates() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    write_all_master(&session.master, b"RAISE 9\n");
    let code = session.wait_exit(Duration::from_secs(10));
    assert_eq!(code, Some(128 + 9));
}

/// The outer terminal's termios is restored to exactly what it was before
/// relay mode touched it, on every exit path (here: a normal `EXIT 0`).
#[test]
fn outer_termios_restored_after_session_ends() {
    let home = TestHome::new();
    // An independent, never-handed-to-a-subprocess pty as the "nothing has
    // touched it yet" (cooked-mode default) baseline — not `session.master`
    // itself: reading its termios immediately after spawn would race
    // `pty_harness`'s own slave-open (see `open_pty` vs `open_pty_seeded`).
    let before = {
        let (m, _) = open_pty_seeded();
        get_termios(&m)
    };
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));
    let during = get_termios(&session.master);
    assert!(
        !termios_eq(&before, &during),
        "raw mode should be active while claude runs"
    );

    write_all_master(&session.master, b"EXIT 0\n");
    let code = session.wait_exit(Duration::from_secs(10));
    assert_eq!(code, Some(0));

    let after = get_termios(&session.master);
    assert!(
        termios_eq(&before, &after),
        "termios must be restored to its pre-raw-mode state after exit"
    );
}

/// Closing every reference to the outer pty's master (simulating the outer
/// terminal hanging up, e.g. an SSH disconnect) delivers `SIGHUP` to csm's
/// foreground process group; the relay forwards it to the leader, which
/// forwards it to claude's process group. `fake_claude_relay` installs no
/// `SIGHUP` handler (default disposition: terminate), so the whole chain's
/// success is observable as csm's own prompt exit with the `128 + SIGHUP`
/// code — if the hangup were not propagated, csm would hang here instead.
#[test]
fn outer_hangup_sends_sighup_to_claude() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    // Replace `session.master` with a throwaway pty so the field stays
    // valid; the *original* master — every reference this process held to
    // the outer pty — is what actually gets dropped (closed) here,
    // triggering the kernel hangup.
    let (throwaway, _) = open_pty();
    let real_master = std::mem::replace(&mut session.master, throwaway);
    drop(real_master);

    let code = session.wait_exit(Duration::from_secs(10));
    assert_eq!(
        code,
        Some(128 + libc::SIGHUP),
        "expected SIGHUP-death exit code, got {code:?}"
    );
}

/// `CSM_SUPERVISOR_PID` is present (and equal to csm's own pid) in claude's
/// environment only in relay mode; direct mode never sets it (and
/// explicitly removes it — see `PosixLauncher`'s `env_remove`).
#[test]
fn supervisor_pid_env_relay_vs_direct() {
    let home = TestHome::new();

    let mut relay = spawn_via_harness(&home, &["-n"], &[]);
    let relay_startup = read_startup(&relay.master, Duration::from_secs(10));
    assert_eq!(
        relay_startup.supervisor.as_deref(),
        Some(relay.pid().to_string().as_str())
    );
    relay.kill_if_alive();

    let mut direct = spawn_via_harness(&home, &["-n"], &[("CSM_RELAY", "0")]);
    let direct_startup = read_startup(&direct.master, Duration::from_secs(10));
    assert_eq!(
        direct_startup.supervisor, None,
        "direct mode must not set CSM_SUPERVISOR_PID"
    );
    direct.kill_if_alive();
}

/// `CSM_RELAY=0` forces direct mode even with a perfectly good controlling
/// terminal: no raw mode, no `__pty-leader` hop (claude is csm's direct
/// child, so the outer terminal's own cooked-mode ECHO is what makes typed
/// input visible — there is no relay to suppress it).
#[test]
fn csm_relay_zero_forces_direct_mode() {
    let home = TestHome::new();
    // FAKE_CLAUDE_NO_RAW: a well-behaved child that never touches termios
    // itself, so any change observed below can only have come from csm's
    // own (direct-mode) launcher code, not from fake_claude_relay mimicking
    // a real interactive CLI's own raw-mode startup.
    let mut session = spawn_via_harness(
        &home,
        &["-n"],
        &[("CSM_RELAY", "0"), ("FAKE_CLAUDE_NO_RAW", "1")],
    );
    let startup = read_startup(&session.master, Duration::from_secs(10));
    assert_eq!(startup.supervisor, None);

    let now = get_termios(&session.master);
    assert!(
        now.local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON),
        "direct mode must never touch termios"
    );
    assert!(
        now.local_flags
            .contains(nix::sys::termios::LocalFlags::ECHO),
        "direct mode must never touch termios"
    );

    session.kill_if_alive();
}

/// `idleCompact: off` in the config keeps the launch direct.
#[test]
fn idle_compact_off_forces_direct_mode() {
    let home = TestHome::new();
    fs::write(
        home.path()
            .join(".config")
            .join("claude-smart")
            .join("config.json"),
        br#"{"idleCompact":"off"}"#,
    )
    .expect("write config.json");
    let mut session = spawn_via_harness(&home, &["-n"], &[("FAKE_CLAUDE_NO_RAW", "1")]);
    let startup = read_startup(&session.master, Duration::from_secs(10));
    assert_eq!(startup.supervisor, None);
    let now = get_termios(&session.master);
    assert!(
        now.local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON),
        "direct mode must never touch termios"
    );
    session.kill_if_alive();
}

/// A non-tty stdin forces direct mode regardless of `CSM_RELAY` (the
/// activation predicate's own `stdin_is_tty` gate) — exercised over plain
/// pipes, no pty at all.
#[test]
fn non_tty_stdin_forces_direct_mode() {
    let home = TestHome::new();
    let mut child = spawn_direct_piped(&home, &["-n"], &[]);
    let stdout = child.stdout.take().expect("piped stdout");

    // Piped stdio has no pty framing (no \r\n, no raw-mode gate needed), but
    // the same poll-based reader works on any readable fd.
    let collected = wait_for(stdout.as_fd(), b"SUPERVISOR", Duration::from_secs(10));
    let lines = lines_of(&collected);
    let supervisor = lines.iter().find(|l| l.starts_with("SUPERVISOR "));
    assert_eq!(
        supervisor.map(String::as_str),
        Some("SUPERVISOR -"),
        "got {lines:?}"
    );

    let mut stdin = child.stdin.take().expect("piped stdin");
    use std::io::Write as _;
    let _ = stdin.write_all(b"EXIT 3\n");
    drop(stdin);
    let start = Instant::now();
    let code = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status.code();
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(code, Some(3));
}

/// The pid csm records (`<sid>.pid`) is claude's *real* pid — cross-checked
/// against `fake_claude_relay`'s own self-reported pid (`READY <pid>`), with
/// the session id recovered from its `ARGV` line (`--session-id <uuid>`,
/// `-n`'s fresh-session path).
#[test]
fn pid_file_matches_fake_claude_real_pid() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    let startup = read_startup(&session.master, Duration::from_secs(10));
    let sid = session_id_from_argv(&startup.argv);

    let pid_file = home.pid_file(&sid);
    let deadline = Instant::now() + Duration::from_secs(5);
    let content = loop {
        if let Ok(s) = fs::read_to_string(&pid_file) {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "pid file never appeared at {pid_file:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let recorded_pid: u32 = content
        .split_whitespace()
        .next()
        .expect("pid field")
        .parse()
        .expect("pid parses");
    assert_eq!(recorded_pid, startup.claude_pid);

    session.kill_if_alive();
}

/// Ctrl-Z-style suspend/resume. The outer terminal is in raw mode (ISIG off
/// there), so a literal `0x1a` byte is relayed to claude's *inner* pty
/// verbatim; the inner pty's slave termios (copied from the outer's
/// original, pre-raw settings) has `ISIG` on, so the kernel itself turns
/// that byte into a real `SIGTSTP` delivered to claude's foreground process
/// group — exactly as a real terminal would. That stops claude; csm's own
/// suspend (`leader_control_thread`'s `raise(Signal::SIGTSTP)`, once it sees
/// claude's `stopped` report) is a *self*-directed stop, and depends on
/// csm's own process group being a real, non-orphaned job — which is exactly
/// what `pty_harness` staying alive as csm's "shell" (rather than an
/// exec-in-place hop) guarantees; see its own doc comment for what an
/// orphaned group would silently do to `SIGTSTP` here instead.
#[test]
fn ctrl_z_suspends_and_sigcont_resumes() {
    let home = TestHome::new();
    let mut session = spawn_via_harness(&home, &["-n"], &[]);
    read_startup(&session.master, Duration::from_secs(10));

    write_all_master(&session.master, &[0x1a]); // Ctrl-Z, raw byte

    let pid = session.pid();
    assert!(
        session.wait_event("STOPPED", Duration::from_secs(10)),
        "csm did not stop"
    );

    killpg(Pid::from_raw(pid as i32), Signal::SIGCONT).expect("SIGCONT the outer pgrp");
    assert!(
        session.wait_event("CONTINUED", Duration::from_secs(10)),
        "csm did not continue"
    );

    // The session must still be fully functional after resume.
    write_all_master(&session.master, b"still alive\n");
    let out = wait_for(
        session.master.as_fd(),
        b"ECHO still alive",
        Duration::from_secs(5),
    );
    assert!(
        lines_of(&out).iter().any(|l| l == "ECHO still alive"),
        "got {:?}",
        lines_of(&out)
    );

    session.kill_if_alive();
}
