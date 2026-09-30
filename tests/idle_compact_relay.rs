//! End-to-end scenarios for idle-compact through the real pty relay: the
//! built `csm` binary runs under a real controlling terminal
//! (`pty_harness`), with `fake_claude_ui` standing in for claude. The fake
//! replays screen streams captured from real Claude Code
//! (`tests/fixtures/screens/`) and records the bytes csm types, so what is
//! asserted is what the supervisor really did: the outcome line in
//! `idle-compact.log`, the bytes the fake received, the marker the fake
//! writes when Enter arrives, and the OSC 777 notification on the outer
//! terminal.
//!
//! Every spawn runs with `HOME` pointed at a fresh temp dir and the
//! `CSM_*`/`ORCA_*`/`ZELLIJ*`/`WEZTERM*`/`CLAUDE_CONFIG_DIR` variables
//! removed. The request file is written by the test (the statusline tick that
//! normally writes it is covered by its own unit tests). The helpers below
//! mirror `tests/pty_relay.rs`'s (integration tests cannot share modules
//! without restructuring that file).

#![cfg(unix)]

use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{PtyMaster, grantpt, posix_openpt, unlockpt};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const SCRUBBED_VARS: &[&str] = &[
    "CSM_EMBEDDED",
    "CSM_HOST_REPLACE",
    "CSM_OAUTH_TOKEN_URL",
    "CSM_ORCA",
    "CSM_RELAY",
    "CSM_STATUSLINE_NO_CAPTURE",
    "CSM_SUPERVISOR_PID",
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
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
];

/// The fixtures the scenarios replay are 120 columns by 40 rows.
const ROWS: u16 = 40;
const COLS: u16 = 120;

struct Scenario {
    /// Fixture rendered when the fake claude starts.
    idle: &'static str,
    /// `FAKE_UI_RULES`: `typed text=fixture` pairs separated by `|`.
    rules: &'static str,
    /// The request's `mode` (`on` or `dry-run`); also the config value.
    mode: &'static str,
    /// The request's `vim_mode`.
    vim: Option<&'static str>,
    /// Seconds from the request being written to its deadline.
    deadline_secs: i64,
    /// Type a real key at the terminal before the request is written.
    keystroke: bool,
    /// How long to wait for an outcome line.
    wait: Duration,
}

impl Scenario {
    fn new(idle: &'static str) -> Self {
        Scenario {
            idle,
            rules: "",
            mode: "on",
            vim: Some("insert"),
            deadline_secs: 60,
            keystroke: false,
            wait: Duration::from_secs(40),
        }
    }
}

const COMPACT_RULES: &str = "/compact=compact-menu-typed-120x40|\\r=compact-started-120x40";

struct Result {
    outcome: String,
    line: String,
    typed: Vec<u8>,
    enter_marker: bool,
    outer: Vec<u8>,
}

impl Result {
    fn notified(&self) -> bool {
        self.outer
            .windows(b"\x1b]777;notify;idle-compact;".len())
            .any(|w| w == b"\x1b]777;notify;idle-compact;")
    }
}

fn open_pty() -> (PtyMaster, String) {
    let master =
        posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).expect("posix_openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let slave_path = unsafe { nix::pty::ptsname(&master) }.expect("ptsname");
    (master, slave_path)
}

/// Collects everything csm writes to the outer terminal until dropped.
struct Drain {
    buf: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drain {
    fn start(master: &PtyMaster) -> Drain {
        let fd: OwnedFd = master.as_fd().try_clone_to_owned().expect("dup master");
        let buf = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, s) = (Arc::clone(&buf), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            while !s.load(Ordering::SeqCst) {
                let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::from(100u16)) {
                    Ok(n) if n > 0 => {}
                    Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => break,
                }
                let revents = fds[0].revents().unwrap_or(PollFlags::empty());
                if revents.contains(PollFlags::POLLIN) {
                    match nix::unistd::read(fd.as_fd(), &mut tmp) {
                        Ok(0) => break,
                        Ok(n) => b.lock().unwrap().extend_from_slice(&tmp[..n]),
                        Err(nix::errno::Errno::EINTR) => {}
                        Err(_) => break,
                    }
                } else if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
                    break;
                }
            }
        });
        Drain {
            buf,
            stop,
            handle: Some(handle),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().unwrap().clone()
    }

    fn wait_for(&self, needle: &[u8], timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.snapshot().windows(needle.len()).any(|w| w == needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Session {
    child: Child,
    csm_pid: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // csm is the harness's child; kill it by the pid the harness reported.
        let _ = kill(Pid::from_raw(self.csm_pid as i32), Signal::SIGKILL);
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("state").join("csm")
}

fn write_request(home: &Path, csm_pid: u32, sc: &Scenario) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut req = serde_json::json!({
        "v": 1,
        "mode": sc.mode,
        "sid": "e2e-sid",
        "written_at": now,
        "deadline": now + sc.deadline_secs,
        "recache_tokens": 150_000,
        "remaining_secs": 120,
    });
    if let Some(v) = sc.vim {
        req["vim_mode"] = serde_json::Value::String(v.to_owned());
    }
    let dir = state_dir(home).join("idle-compact-requests");
    fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join(format!("{csm_pid}.tmp"));
    fs::write(&tmp, serde_json::to_vec(&req).unwrap()).unwrap();
    fs::rename(&tmp, dir.join(format!("{csm_pid}.json"))).unwrap();
}

fn outcome_line(home: &Path) -> Option<String> {
    fs::read_to_string(state_dir(home).join("idle-compact.log"))
        .ok()?
        .lines()
        .find(|l| l.contains("outcome="))
        .map(str::to_owned)
}

fn received_bytes(path: &Path) -> Vec<u8> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .flat_map(|l| {
            (0..l.len() / 2)
                .map(|i| u8::from_str_radix(&l[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn run(sc: Scenario) -> Result {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let cfg = home.join(".config").join("claude-smart");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("config.json"),
        format!(r#"{{"idleCompact":"{}"}}"#, sc.mode),
    )
    .unwrap();
    let log = dir.path().join("typed.log");
    let marker = dir.path().join("enter.marker");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/screens");

    let (master, slave_path) = open_pty();
    let drain = Drain::start(&master);
    let harness = std::env::var("CARGO_BIN_EXE_pty_harness").expect("pty_harness");
    let csm = std::env::var("CARGO_BIN_EXE_csm").expect("csm");
    let fake = std::env::var("CARGO_BIN_EXE_fake_claude_ui").expect("fake_claude_ui");

    let mut cmd = Command::new(harness);
    cmd.arg(&slave_path).arg(&csm).arg("-n");
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if !SCRUBBED_VARS.contains(&k.as_str()) && k != "HOME" {
            cmd.env(k, v);
        }
    }
    cmd.env("HOME", &home)
        .env("PTY_HARNESS_SIZE", format!("{ROWS}x{COLS}"))
        .env("CLAUDE_SMART_CLAUDE_BIN", &fake)
        .env("FAKE_UI_DIR", &fixtures)
        .env("FAKE_UI_IDLE", sc.idle)
        .env("FAKE_UI_RULES", sc.rules)
        .env("FAKE_UI_LOG", &log)
        .env("FAKE_UI_MARKER", &marker);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn pty_harness");
    use std::io::BufRead as _;
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).expect("PID line");
    let csm_pid: u32 = line.trim().strip_prefix("PID ").unwrap().parse().unwrap();
    let session = Session { child, csm_pid };

    assert!(
        drain.wait_for(b"READY", Duration::from_secs(60)),
        "the fake claude never started"
    );
    // Let the first screen finish drawing before the request arrives.
    std::thread::sleep(Duration::from_millis(500));
    if sc.keystroke {
        let fd = master.as_fd();
        nix::unistd::write(fd, b"x").expect("type a key");
        std::thread::sleep(Duration::from_millis(500));
    }
    write_request(&home, session.csm_pid, &sc);

    let start = Instant::now();
    let mut line = None;
    while start.elapsed() < sc.wait {
        line = outcome_line(&home);
        if line.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let line = line.unwrap_or_else(|| {
        panic!(
            "no outcome line within {:?}; typed so far {:?}",
            sc.wait,
            received_bytes(&log)
        )
    });
    // Give a late notification a moment to reach the outer terminal.
    std::thread::sleep(Duration::from_millis(1500));
    let outcome = line
        .split("outcome=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap()
        .to_owned();
    Result {
        outcome,
        line,
        typed: received_bytes(&log),
        enter_marker: marker.exists(),
        outer: drain.snapshot(),
    }
}

#[test]
fn happy_path_types_compact_and_presses_enter() {
    let mut sc = Scenario::new("idle-after-turn-120x40");
    sc.rules = COMPACT_RULES;
    let r = run(sc);
    assert_eq!(
        r.outcome, "delivered",
        "{} typed={:?} marker={}",
        r.line, r.typed, r.enter_marker
    );
    assert_eq!(r.typed, b"/compact\r");
    assert!(r.enter_marker, "Enter must have reached the fake");
    assert!(r.line.contains("box="), "{}", r.line);
    assert!(!r.notified(), "a clean delivery notifies nobody");
}

#[test]
fn vim_normal_gets_i_then_compact_then_enter() {
    let mut sc = Scenario::new("vim-normal-120x40");
    sc.vim = Some("normal");
    sc.rules = "i=vim-insert-120x40|/compact=compact-menu-typed-120x40|\\r=compact-started-120x40";
    let r = run(sc);
    assert_eq!(
        r.outcome, "delivered",
        "{} typed={:?} marker={}",
        r.line, r.typed, r.enter_marker
    );
    assert_eq!(r.typed, b"i/compact\r");
    assert!(r.enter_marker);
}

#[test]
fn draft_is_never_typed_over() {
    let mut sc = Scenario::new("draft-hello-120x40");
    sc.rules = COMPACT_RULES;
    let r = run(sc);
    assert_eq!(r.outcome, "draft", "{}", r.line);
    assert!(r.typed.is_empty(), "typed {:?}", r.typed);
    assert!(!r.enter_marker);
    assert!(r.notified(), "the user is told the draft blocked it");
}

#[test]
fn multiline_and_pasted_drafts_are_drafts_too() {
    for idle in [
        "draft-multiline-120x40",
        "draft-wrapped-120x40",
        "draft-paste-placeholder-120x40",
    ] {
        let mut sc = Scenario::new(idle);
        sc.rules = COMPACT_RULES;
        let r = run(sc);
        assert_eq!(r.outcome, "draft", "{idle}: {}", r.line);
        assert!(r.typed.is_empty(), "{idle}: typed {:?}", r.typed);
    }
}

#[test]
fn dialog_screen_gets_nothing_until_the_deadline() {
    let mut sc = Scenario::new("ask-user-question-dialog-120x40");
    sc.rules = COMPACT_RULES;
    sc.deadline_secs = 6;
    let r = run(sc);
    assert_eq!(r.outcome, "expired", "{}", r.line);
    assert!(r.typed.is_empty(), "typed {:?}", r.typed);
}

#[test]
fn recent_keystroke_blocks_typing() {
    let mut sc = Scenario::new("idle-after-turn-120x40");
    sc.rules = COMPACT_RULES;
    sc.deadline_secs = 6;
    sc.keystroke = true;
    let r = run(sc);
    assert_eq!(r.outcome, "expired", "{}", r.line);
    assert_eq!(r.typed, b"x", "only the user's own key may arrive");
}

#[test]
fn busy_screen_gets_nothing() {
    let mut sc = Scenario::new("busy-generating-120x40");
    sc.rules = COMPACT_RULES;
    sc.deadline_secs = 6;
    let r = run(sc);
    assert_eq!(r.outcome, "vetoed-screen-busy", "{}", r.line);
    assert!(r.typed.is_empty(), "typed {:?}", r.typed);
}

#[test]
fn request_vim_mismatch_gets_nothing() {
    // The request says INSERT but the screen shows no INSERT marker.
    let mut sc = Scenario::new("vim-normal-120x40");
    sc.vim = Some("insert");
    sc.rules = COMPACT_RULES;
    sc.deadline_secs = 6;
    let r = run(sc);
    assert_eq!(r.outcome, "expired", "{}", r.line);
    assert!(r.typed.is_empty(), "typed {:?}", r.typed);
}

#[test]
fn dry_run_types_nothing_and_logs_what_it_would_do() {
    let mut sc = Scenario::new("idle-after-turn-120x40");
    sc.mode = "dry-run";
    sc.rules = COMPACT_RULES;
    let r = run(sc);
    assert_eq!(r.outcome, "dry-run-would-type", "{}", r.line);
    assert!(r.typed.is_empty(), "typed {:?}", r.typed);
    assert!(!r.enter_marker);
    assert!(!r.notified(), "dry-run notifies nobody");
    assert!(r.line.contains("box=empty"), "{}", r.line);
    assert!(r.line.contains("vim=insert"), "{}", r.line);
}

#[test]
fn verify_failure_rolls_back_the_typed_text() {
    let mut sc = Scenario::new("idle-after-turn-120x40");
    // Whatever is typed, the "claude" shows some other text in the box.
    sc.rules = "/compact=draft-hello-120x40";
    let r = run(sc);
    assert_eq!(r.outcome, "verify-failed", "{}", r.line);
    let mut want = b"/compact".to_vec();
    want.extend(std::iter::repeat_n(0x7f, 8));
    assert_eq!(r.typed, want, "typed text erased with one DEL per char");
    assert!(!r.enter_marker, "Enter must not be pressed");
    assert!(r.notified());
}
