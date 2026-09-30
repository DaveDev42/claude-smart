//! Test-only stand-in for `claude`'s screen, driven by
//! `tests/idle_compact_relay.rs` through `CLAUDE_SMART_CLAUDE_BIN`. It replays
//! byte streams captured from a real Claude Code session
//! (`tests/fixtures/screens/*.bin`) so the idle-compact supervisor sees the
//! real escape sequences, and it records what csm types at it. Never built
//! into a release artifact; a no-op on non-unix.
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
//!
//! On start it prints `READY <pid>` on its own line first (before the
//! fixture), so a test can wait for it.

#[cfg(unix)]
fn main() {
    use std::io::{Read, Write};

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

#[cfg(not(unix))]
fn main() {}
