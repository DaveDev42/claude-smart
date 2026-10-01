//! Windows counterpart of `tests/idle_compact_relay.rs`: the built `csm` runs
//! inside a pseudoconsole the test controls (`conpty_harness`, standing in
//! for Windows Terminal), csm starts its own ConPTY relay, and
//! `fake_claude_ui` stands in for claude, replaying the captured screens and
//! recording the bytes it receives. Asserted: the outcome line in
//! `idle-compact.log`, the bytes the fake received, the marker the fake
//! writes when Enter arrives, the exit code, and the size the fake sees.
//!
//! Needs the `e2e` feature: only a sandbox build of csm keeps its home inside
//! the test's temp dir on Windows (`dirs::home_dir` ignores `HOME` and
//! `USERPROFILE` there; see `paths::home_dir`). Run it with
//! `cargo test --features e2e --test idle_compact_conpty`.

#![cfg(all(windows, feature = "e2e"))]

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    "XDG_DATA_HOME",
];

/// The fixtures the scenarios replay are 120 columns by 40 rows.
const ROWS: u16 = 40;
const COLS: u16 = 120;

const COMPACT_RULES: &str = "/compact=compact-menu-typed-120x40|\\r=compact-started-120x40";

/// One csm session inside the harness's pseudoconsole.
struct Session {
    _dir: tempfile::TempDir,
    home: PathBuf,
    typed_log: PathBuf,
    marker: PathBuf,
    size_log: PathBuf,
    child: Child,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
    csm_pid: u32,
}

struct Opts {
    idle: &'static str,
    rules: &'static str,
    mode: &'static str,
    exit_rule: Option<&'static str>,
    extra_env: Vec<(&'static str, &'static str)>,
}

impl Opts {
    fn new(idle: &'static str) -> Self {
        Opts {
            idle,
            rules: COMPACT_RULES,
            mode: "on",
            exit_rule: None,
            extra_env: Vec::new(),
        }
    }
}

impl Session {
    fn start(o: Opts) -> Session {
        let dir = tempfile::tempdir().expect("tempdir");
        let sandbox = dir.path().to_path_buf();
        let home = sandbox.join("home");
        let cfg = home.join(".config").join("claude-smart");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(
            cfg.join("config.json"),
            format!(r#"{{"idleCompact":"{}"}}"#, o.mode),
        )
        .unwrap();
        let local = home.join("AppData").join("Local");
        let roaming = home.join("AppData").join("Roaming");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&roaming).unwrap();
        let typed_log = sandbox.join("typed.log");
        let marker = sandbox.join("enter.marker");
        let size_log = sandbox.join("size.log");
        let pid_file = sandbox.join("csm.pid");
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/screens");

        let harness = env!("CARGO_BIN_EXE_conpty_harness");
        let csm = env!("CARGO_BIN_EXE_csm");
        let fake = env!("CARGO_BIN_EXE_fake_claude_ui");

        let mut cmd = Command::new(harness);
        cmd.arg(csm).arg("-n");
        for k in SCRUBBED_VARS {
            cmd.env_remove(k);
        }
        cmd.env("CSM_E2E_SANDBOX", &sandbox)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", &local)
            .env("APPDATA", &roaming)
            .env("CONPTY_HARNESS_SIZE", format!("{ROWS}x{COLS}"))
            .env("CONPTY_HARNESS_PID_FILE", &pid_file)
            .env("CLAUDE_SMART_CLAUDE_BIN", fake)
            .env("FAKE_UI_DIR", &fixtures)
            .env("FAKE_UI_IDLE", o.idle)
            .env("FAKE_UI_RULES", o.rules)
            .env("FAKE_UI_LOG", &typed_log)
            .env("FAKE_UI_MARKER", &marker)
            .env("FAKE_UI_SIZE_LOG", &size_log);
        if let Some(rule) = o.exit_rule {
            cmd.env("FAKE_UI_EXIT", rule);
        }
        for (k, v) in &o.extra_env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn conpty_harness");
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        {
            let out = Arc::clone(&out);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
            });
        }
        let start = Instant::now();
        let csm_pid = loop {
            if let Some(p) = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "csm never started"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        Session {
            _dir: dir,
            home,
            typed_log,
            marker,
            size_log,
            child,
            stdin,
            out,
            csm_pid,
        }
    }

    fn command(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("harness stdin");
        writeln!(stdin, "{line}").expect("harness command");
        stdin.flush().unwrap();
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.command(&format!("W {hex}"));
    }

    fn output(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    fn wait_output(&self, needle: &[u8], timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.output().windows(needle.len()).any(|w| w == needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Wait for the fake claude to be up and its first screen drawn.
    fn wait_ready(&self) {
        assert!(
            self.wait_output(b"READY", Duration::from_secs(60)),
            "the fake claude never started; output {:?}",
            String::from_utf8_lossy(&self.output())
        );
        std::thread::sleep(Duration::from_millis(800));
    }

    fn state_dir(&self) -> PathBuf {
        self.home.join("AppData").join("Local").join("csm")
    }

    fn write_request(&self, mode: &str, deadline_secs: i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let req = serde_json::json!({
            "v": 1,
            "mode": mode,
            "sid": "e2e-sid",
            "written_at": now,
            "deadline": now + deadline_secs,
            "recache_tokens": 150_000,
            "remaining_secs": 120,
            "vim_mode": "insert",
        });
        let dir = self.state_dir().join("idle-compact-requests");
        fs::create_dir_all(&dir).unwrap();
        let pid = self.csm_pid;
        let tmp = dir.join(format!("{pid}.tmp"));
        fs::write(&tmp, serde_json::to_vec(&req).unwrap()).unwrap();
        fs::rename(&tmp, dir.join(format!("{pid}.json"))).unwrap();
    }

    fn outcome_line(&self, timeout: Duration) -> String {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(l) = fs::read_to_string(self.state_dir().join("idle-compact.log"))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.contains("outcome="))
                        .map(str::to_owned)
                })
            {
                return l;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "no outcome line within {timeout:?}; typed so far {:?}",
            self.typed()
        );
    }

    fn typed(&self) -> Vec<u8> {
        let Ok(text) = fs::read_to_string(&self.typed_log) else {
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

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Close the harness's pseudoconsole: csm gets a close event, closes
        // its own, and the fake claude goes with it.
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Some(stdin) = self.stdin.as_mut() {
                let _ = writeln!(stdin, "Q");
                let _ = stdin.flush();
            }
            if self.wait_exit(Duration::from_secs(10)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

fn outcome_word(line: &str) -> String {
    line.split("outcome=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap()
        .to_owned()
}

fn run_request(o: Opts, deadline_secs: i64, wait: Duration) -> (Session, String) {
    let mode = o.mode;
    let s = Session::start(o);
    s.wait_ready();
    s.write_request(mode, deadline_secs);
    let line = s.outcome_line(wait);
    // Let anything still in flight land before the assertions read it.
    std::thread::sleep(Duration::from_millis(1500));
    (s, line)
}

#[test]
fn happy_path_types_compact_and_presses_enter() {
    let (s, line) = run_request(
        Opts::new("idle-after-turn-120x40"),
        60,
        Duration::from_secs(40),
    );
    assert_eq!(
        outcome_word(&line),
        "delivered",
        "{line} typed={:?}",
        s.typed()
    );
    assert_eq!(s.typed(), b"/compact\r");
    assert!(s.marker.exists(), "Enter must have reached the fake");
    assert!(line.contains("box="), "{line}");
}

#[test]
fn draft_is_never_typed_over() {
    let (s, line) = run_request(Opts::new("draft-hello-120x40"), 60, Duration::from_secs(40));
    assert_eq!(outcome_word(&line), "draft", "{line}");
    assert!(s.typed().is_empty(), "typed {:?}", s.typed());
    assert!(!s.marker.exists());
}

#[test]
fn busy_screen_gets_nothing() {
    let (s, line) = run_request(
        Opts::new("busy-generating-120x40"),
        6,
        Duration::from_secs(40),
    );
    assert_eq!(outcome_word(&line), "vetoed-screen-busy", "{line}");
    assert!(s.typed().is_empty(), "typed {:?}", s.typed());
}

#[test]
fn dry_run_types_nothing_and_logs_what_it_would_do() {
    let mut o = Opts::new("idle-after-turn-120x40");
    o.mode = "dry-run";
    let (s, line) = run_request(o, 60, Duration::from_secs(40));
    assert_eq!(outcome_word(&line), "dry-run-would-type", "{line}");
    assert!(s.typed().is_empty(), "typed {:?}", s.typed());
    assert!(!s.marker.exists());
    assert!(line.contains("box=empty"), "{line}");
}

#[test]
fn keys_and_ctrl_c_reach_claude_as_input() {
    let mut s = Session::start(Opts::new("idle-after-turn-120x40"));
    s.wait_ready();
    s.type_bytes(b"ab\x03");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && s.typed() != b"ab\x03" {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(s.typed(), b"ab\x03", "Ctrl-C must arrive as a 0x03 key");
    assert!(
        matches!(s.child.try_wait(), Ok(None)),
        "Ctrl-C must not end csm"
    );
}

#[test]
fn exit_code_propagates() {
    let mut o = Opts::new("idle-after-turn-120x40");
    o.exit_rule = Some("q=7");
    let mut s = Session::start(o);
    s.wait_ready();
    s.type_bytes(b"q");
    assert_eq!(
        s.wait_exit(Duration::from_secs(30)),
        Some(7),
        "claude's exit code must become csm's"
    );
}

#[test]
fn resize_propagates_to_the_inner_console() {
    let mut s = Session::start(Opts::new("idle-after-turn-120x40"));
    s.wait_ready();
    let sizes = |s: &Session| fs::read_to_string(&s.size_log).unwrap_or_default();
    assert!(
        sizes(&s).lines().any(|l| l == format!("{ROWS} {COLS}")),
        "initial size: {:?}",
        sizes(&s)
    );
    s.command("R 30 100");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && !sizes(&s).lines().any(|l| l == "30 100") {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        sizes(&s).lines().any(|l| l == "30 100"),
        "resized: {:?}",
        sizes(&s)
    );
}

#[test]
fn csm_relay_zero_keeps_the_direct_launcher() {
    let mut o = Opts::new("idle-after-turn-120x40");
    o.extra_env.push(("CSM_RELAY", "0"));
    let s = Session::start(o);
    s.wait_ready();
    s.write_request("on", 5);
    // No relay means no supervisor: the request is never picked up.
    std::thread::sleep(Duration::from_secs(8));
    assert!(
        !s.state_dir().join("idle-compact.log").exists(),
        "a direct launch has no supervisor to log an outcome"
    );
    assert!(s.typed().is_empty());
}
