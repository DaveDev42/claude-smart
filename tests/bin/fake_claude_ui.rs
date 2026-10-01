//! Test-only stand-in for `claude`'s screen, driven by
//! `tests/idle_compact_relay.rs` through `CLAUDE_SMART_CLAUDE_BIN`. It replays
//! byte streams captured from a real Claude Code session
//! (`tests/fixtures/screens/*.bin`) so the idle-compact supervisor sees the
//! real escape sequences, and it records what csm types at it. Never built
//! into a release artifact. Runs on unix (under the pty relay) and on Windows
//! (inside the ConPTY relay, `tests/idle_compact_conpty.rs`).
//!
//! Environment (all set by the test):
//!
//! - `FAKE_UI_DIR`: the fixtures directory.
//! - `FAKE_UI_IDLE`: the fixture name rendered at start.
//! - `FAKE_UI_RULES`: `trigger=fixture` pairs separated by `|`. `trigger`
//!   is literal text (`\r` and `\x7f` escapes allowed). Whenever the bytes
//!   received since the last render end with a trigger, that fixture is
//!   replayed. A trigger of `\r` also writes `FAKE_UI_MARKER` (the file the
//!   test checks to prove Enter arrived).
//! - `FAKE_UI_LOG`: every chunk read from stdin is appended here, as hex.
//! - `FAKE_UI_MARKER`: see above.
//! - `FAKE_UI_EXIT`: `trigger=code`; when the bytes received end with
//!   `trigger`, exit with `code` (Windows tests check exit code propagation).
//! - `FAKE_UI_SIZE_LOG` (Windows only): every console size seen, polled every
//!   50 ms, appended as `<rows> <cols>` lines.
//!
//! On start it prints `READY <pid>` on its own line first (before the
//! fixture), so a test can wait for it.

#[cfg(unix)]
fn main() {
    // Non-canonical, no echo, no CR translation: bytes arrive as typed, like
    // the real UI's raw mode. ISIG stays so Ctrl-C still works.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            t.c_lflag &= !((libc::ECHO | libc::ICANON) as libc::tcflag_t);
            // Enter must arrive as a bare CR, as in the real UI's raw mode.
            t.c_iflag &= !((libc::ICRNL | libc::INLCR) as libc::tcflag_t);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
    run();
}

/// Windows: raw VT console mode, like Claude Code's own. No line input, no
/// echo, Ctrl-C as a 0x03 byte; VT sequences on output are interpreted.
#[cfg(windows)]
fn main() {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let hin = GetStdHandle(STD_INPUT_HANDLE);
        let mut m = 0u32;
        if GetConsoleMode(hin, &mut m) != 0 {
            m &= !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            SetConsoleMode(hin, m | ENABLE_VIRTUAL_TERMINAL_INPUT);
        }
        let hout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut m = 0u32;
        if GetConsoleMode(hout, &mut m) != 0 {
            SetConsoleMode(hout, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
    if let Ok(path) = std::env::var("FAKE_UI_SIZE_LOG") {
        std::thread::spawn(move || size_logger(&path));
    }
    run();
}

#[cfg(windows)]
fn size_logger(path: &str) {
    use std::io::Write;
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };
    let mut last = (0i32, 0i32);
    loop {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } != 0 {
            let w = info.srWindow;
            let size = (
                i32::from(w.Bottom) - i32::from(w.Top) + 1,
                i32::from(w.Right) - i32::from(w.Left) + 1,
            );
            if size != last {
                last = size;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{} {}", size.0, size.1);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(any(unix, windows))]
fn run() {
    use std::io::{Read, Write};

    let dir = std::path::PathBuf::from(std::env::var("FAKE_UI_DIR").unwrap_or_default());
    let idle = std::env::var("FAKE_UI_IDLE").unwrap_or_default();
    let log = std::env::var("FAKE_UI_LOG").ok();
    let marker = std::env::var("FAKE_UI_MARKER").ok();
    let unescape = |s: &str| s.replace("\\r", "\r").replace("\\x7f", "\x7f");
    let rules: Vec<(Vec<u8>, String)> = std::env::var("FAKE_UI_RULES")
        .unwrap_or_default()
        .split('|')
        .filter_map(|p| p.split_once('='))
        .map(|(t, f)| (unescape(t).into_bytes(), f.to_owned()))
        .collect();
    let exit_rule: Option<(Vec<u8>, i32)> = std::env::var("FAKE_UI_EXIT").ok().and_then(|v| {
        let (t, c) = v.rsplit_once('=')?;
        Some((unescape(t).into_bytes(), c.parse().ok()?))
    });

    let render = |name: &str| {
        let bytes = std::fs::read(dir.join(format!("{name}.bin"))).unwrap_or_default();
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    };

    println!("READY {}", std::process::id());
    let _ = std::io::stdout().flush();
    render(&idle);

    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 256];
    let mut stdin = std::io::stdin().lock();
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(path) = &log {
            let hex: String = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{hex}");
            }
        }
        pending.extend_from_slice(&buf[..n]);
        if let Some((trigger, code)) = &exit_rule
            && pending.ends_with(trigger)
        {
            std::process::exit(*code);
        }
        for (trigger, fixture) in &rules {
            if pending.ends_with(trigger) {
                if trigger == b"\r"
                    && let Some(m) = &marker
                {
                    let _ = std::fs::write(m, b"enter");
                }
                render(fixture);
                pending.clear();
                break;
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn main() {}
