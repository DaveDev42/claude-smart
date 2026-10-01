//! Test-only stand-in for `claude`, driven by `tests/pty_relay.rs` through
//! `CLAUDE_SMART_CLAUDE_BIN` (the same override csm's own launch-command
//! resolver reads — see `crate::config::launch_command_for_spawn`). Never
//! built into a release artifact (see the `[[bin]]` comment in `Cargo.toml`);
//! a no-op on non-unix so `cargo build --all-targets` / clippy stay green on
//! every target even though the relay itself is unix-only.
//!
//! On start, prints these lines to its own stdout:
//!
//!   - `READY <pid>\n` — so a test can confirm it is alive and cross-check
//!     the pid csm records (pidfile, `ChildHandle`) against the real one.
//!   - `ARGV <args...>\n` — its own argv (space-joined; csm appends
//!     `--session-id <uuid>` or `--resume <uuid>` when building the launch
//!     command, so a test can recover the session id purely by observing
//!     this process's output, without reaching into csm's internals).
//!   - `SUPERVISOR <value>\n` or `SUPERVISOR -\n` — `CSM_SUPERVISOR_PID` from
//!     its own environment, or `-` if unset (relay mode sets it, direct mode
//!     removes it; see `crate::platform::posix::PosixLauncher`).
//!
//!   - `SIZE <rows> <cols>\n` — the window size of its own controlling
//!     terminal at start, so a test can check the size csm gave the inner pty.
//!
//! It then echoes every line read from its stdin back to stdout prefixed
//! with `ECHO `, except for a small set of control lines a test can send it:
//!
//!   - `EXIT <code>\n`  — exit with the given code.
//!   - `RAISE <n>\n`    — raise signal `n` on itself (e.g. a segfault-style
//!     abnormal death, distinct from an outer-terminal hangup).
//!
//! `SIGWINCH` prints `RESIZED <rows> <cols>\n` (reading its own controlling
//! terminal's size), so a resize test can confirm the size that reached the
//! *outer* terminal actually propagated all the way to what claude sees.
//! Every other signal (notably `SIGHUP`, sent when the outer terminal hangs
//! up) is left at its default disposition, so the process just dies from it
//! — observable from outside as a signal-terminated `ExitStatus`.
//!
//! On startup, also puts its own controlling terminal into raw mode (no
//! ECHO/ICANON), matching how the real `claude` behaves as an interactive
//! CLI — unless `FAKE_CLAUDE_NO_RAW` is set, which a test can use to keep a
//! plain, well-behaved (never touches termios itself) child process, e.g.
//! to observe csm's own launcher code's termios behavior in isolation.

#[cfg(unix)]
fn main() {
    use std::io::{BufRead, Write};

    unsafe {
        libc::signal(
            libc::SIGWINCH,
            handle_sigwinch as *const () as libc::sighandler_t,
        );
    }

    // A real interactive CLI (the actual `claude`) puts its own controlling
    // terminal into no-echo mode on startup; without this, the inner pty's
    // slave keeps whatever termios the relay copied onto it from the *outer*
    // terminal (ECHO on, before the relay's own raw mode is entered there),
    // so the kernel would echo every typed byte back on the inner pty in
    // addition to this fixture's own explicit `ECHO ...` reply — a double
    // echo that is an artifact of this test fixture, not the relay's own
    // raw-mode handling (which governs the *outer* terminal). Only ECHO is
    // cleared here, deliberately NOT the rest of `cfmakeraw` (ISIG/ICANON):
    // the inner pty's termios keeps ISIG (inherited from the outer
    // terminal's pre-raw settings, by design — see `try_run_foreground`),
    // which is what makes a raw Ctrl-Z byte relayed onto it generate a real
    // `SIGTSTP` to this process's group via the kernel's own VSUSP handling;
    // clearing it here would silently break that suspend/resume path.
    // Skippable (`FAKE_CLAUDE_NO_RAW`) for a test that wants a child which
    // never touches termios itself, to observe csm's own behavior in
    // isolation (e.g. proving direct mode's launcher never touches it).
    if std::env::var_os("FAKE_CLAUDE_NO_RAW").is_none() {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                t.c_lflag &= !(libc::ECHO as libc::tcflag_t);
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }

    let pid = std::process::id();
    println!("READY {pid}");
    let argv: Vec<String> = std::env::args().skip(1).collect();
    println!("ARGV {}", argv.join(" "));
    match std::env::var("CSM_SUPERVISOR_PID") {
        Ok(v) => println!("SUPERVISOR {v}"),
        Err(_) => println!("SUPERVISOR -"),
    }
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ as _, &mut ws as *mut libc::winsize) == 0 {
            println!("SIZE {} {}", ws.ws_row, ws.ws_col);
        }
    }
    let _ = std::io::stdout().flush();

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("EXIT ") {
            let code: i32 = rest.trim().parse().unwrap_or(0);
            std::process::exit(code);
        }
        if let Some(rest) = line.strip_prefix("RAISE ") {
            if let Ok(n) = rest.trim().parse::<i32>() {
                unsafe {
                    libc::raise(n);
                }
            }
            continue;
        }
        println!("ECHO {line}");
        let _ = std::io::stdout().flush();
    }
}

#[cfg(unix)]
extern "C" fn handle_sigwinch(_sig: libc::c_int) {
    // Async-signal-safety: TIOCGWINSZ + a raw write(2) are both safe here
    // (no allocation, no locking beyond what the kernel does internally for
    // the ioctl itself); this handler exists only for this test fixture.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ as _, &mut ws as *mut libc::winsize) == 0 {
            let mut buf = [0u8; 32];
            let msg = format_resize(&mut buf, ws.ws_row, ws.ws_col);
            libc::write(1, msg.as_ptr() as *const libc::c_void, msg.len());
        }
    }
}

/// Format `RESIZED <rows> <cols>\n` into `buf` without allocating (signal-
/// handler safe), returning the written slice.
#[cfg(unix)]
fn format_resize(buf: &mut [u8; 32], rows: u16, cols: u16) -> &[u8] {
    use std::io::Write;
    let mut cursor = &mut buf[..];
    let _ = writeln!(cursor, "RESIZED {rows} {cols}");
    let remaining = cursor.len();
    let written = buf.len() - remaining;
    &buf[..written]
}

#[cfg(not(unix))]
fn main() {}
