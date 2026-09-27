//! Feeding Orca's live usage from `csm statusline` (design §8 "Feeding
//! Orca's live usage"; a port of Orca's managed statusLine script, MH:898).
//!
//! Orca's hook installer leaves a user `statusLine` alone, so with
//! `csm statusline` installed Orca would never receive `/statusline/claude`
//! posts and its limit UI would go stale. csm forwards the payload the way
//! Orca's script does:
//!
//! - skip when `CLAUDE_JOB_DIR` is set or the payload lacks `"rate_limits"`;
//! - read `ORCA_AGENT_HOOK_ENDPOINT` when readable (`KEY=VALUE` lines, or
//!   `set KEY=VALUE` on Windows; a file value wins over the env);
//! - require `ORCA_AGENT_HOOK_PORT`, `ORCA_AGENT_HOOK_TOKEN` and
//!   `ORCA_PANE_KEY`;
//! - honour the 15 s stamp file Orca's script uses for the same pane, so csm
//!   and the script share one throttle;
//! - POST `http://127.0.0.1:<port>/statusline/claude`, form-encoded
//!   `paneKey`, `configDir`, `env`, `version` and `payload`, with the header
//!   `X-Orca-Agent-Hook-Token`, 0.5 s to connect and 1.5 s in all.
//!
//! The gate, the throttle and the stamp run in the statusline process (a
//! small file read and write); only the POST runs in a detached child
//! (`csm statusline --orca-forward`, payload on stdin), after the segment is
//! printed, so render latency does not grow. The child reads the port and
//! token itself from its environment and the endpoint file: the token never
//! appears on a command line, in a log or in an error.
//!
//! Everything but [`spawn_forward`], [`gate_and_stamp`] and [`post`] is pure.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::SecretString;

/// The argument `csm statusline` takes in its forwarding child.
pub const FORWARD_ARG: &str = "--orca-forward";
/// Orca's receiver path (M:12862).
pub const STATUSLINE_PATH: &str = "/statusline/claude";
/// Orca's script posts at most once per this many seconds per pane.
pub const THROTTLE_SECS: i64 = 15;
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
pub const TOTAL_TIMEOUT: Duration = Duration::from_millis(1500);
/// The payload cap (the statusline's own stdin cap).
pub const PAYLOAD_CAP: u64 = 256 * 1024;

// ─── pure core ────────────────────────────────────────────────────────────────

/// Where and how to post. `Debug` never shows the token.
#[derive(Debug, Clone)]
pub struct Coords {
    pub port: u16,
    pub token: SecretString,
    pub env: String,
    pub version: String,
    pub pane_key: String,
    /// `$CLAUDE_CONFIG_DIR`, empty when unset.
    pub config_dir: String,
}

/// Parse an endpoint file: `KEY=VALUE` or `set KEY=VALUE` lines, keys of
/// `[A-Z0-9_]`, a trailing CR stripped. Pure.
pub fn parse_endpoint(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rest = match line.strip_prefix("set") {
            Some(r) if r.starts_with(char::is_whitespace) => r.trim_start(),
            _ => line,
        };
        let Some((key, value)) = rest.split_once('=') else {
            continue;
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        out.insert(key.to_owned(), value.to_owned());
    }
    out
}

/// The post coordinates from the env and the endpoint file's entries, or
/// `None` when the port, the token or the pane key is missing. Pure.
pub fn coords(
    get: &dyn Fn(&str) -> Option<String>,
    endpoint: &HashMap<String, String>,
) -> Option<Coords> {
    let pick = |k: &str| {
        endpoint
            .get(k)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| get(k).filter(|v| !v.is_empty()))
    };
    let port = pick("ORCA_AGENT_HOOK_PORT")?.trim().parse::<u16>().ok()?;
    let token = pick("ORCA_AGENT_HOOK_TOKEN")?;
    let pane_key = get("ORCA_PANE_KEY").filter(|v| !v.is_empty())?;
    Some(Coords {
        port,
        token: SecretString::new(token),
        env: pick("ORCA_AGENT_HOOK_ENV").unwrap_or_default(),
        version: pick("ORCA_AGENT_HOOK_VERSION").unwrap_or_default(),
        pane_key,
        config_dir: get("CLAUDE_CONFIG_DIR").unwrap_or_default(),
    })
}

/// The payload gate: not a background job, and the payload carries
/// `"rate_limits"`. Pure.
pub fn payload_wanted(get: &dyn Fn(&str) -> Option<String>, payload: &str) -> bool {
    get("CLAUDE_JOB_DIR").is_none_or(|v| v.is_empty())
        && !payload.is_empty()
        && payload.contains("\"rate_limits\"")
}

/// The pane id in the stamp file name, derived exactly as Orca's scripts do.
/// POSIX: the part after the last `:`; when that is all digits and the part
/// before it is `[A-Za-z0-9._-]+`, `<tab>_<pane>`. Windows: the last 36
/// characters with `:` turned into `_`. Pure.
pub fn pane_id(pane_key: &str, windows: bool) -> String {
    if windows {
        let chars: Vec<char> = pane_key.chars().collect();
        let tail: String = chars[chars.len().saturating_sub(36)..].iter().collect();
        return tail.replace(':', "_");
    }
    let pane = pane_key.rsplit(':').next().unwrap_or(pane_key);
    if !pane.is_empty() && pane.bytes().all(|b| b.is_ascii_digit()) {
        let tab = pane_key
            .rsplit_once(':')
            .map(|(t, _)| t)
            .unwrap_or(pane_key);
        let tab_ok = !tab.is_empty()
            && tab
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        // A key with no `:` is its own tab part (`${KEY%:*}` leaves it
        // whole), so a bare numeric key `42` becomes `42_42`, as in Orca.
        if tab_ok {
            return format!("{tab}_{pane}");
        }
    }
    pane.to_owned()
}

/// The stamp file for a pane. Pure.
pub fn stamp_path(tmp: &Path, pane_id: &str, windows: bool) -> PathBuf {
    if windows {
        tmp.join(format!("orca-claude-statusline-last-{pane_id}.tmp"))
    } else {
        tmp.join(format!("orca-claude-statusline-last-{pane_id}"))
    }
}

/// A canonical decimal of at most 15 digits.
fn short_decimal(s: &str) -> Option<i64> {
    let ok = !s.is_empty()
        && s.len() <= 15
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s == "0" || !s.starts_with('0'));
    ok.then(|| s.parse().ok()).flatten()
}

/// The POSIX script's clock: the payload's `total_duration_ms` / 1000 when
/// it is a decimal of at most 15 digits, else `epoch_now`. Pure.
pub fn posix_now(payload: &str, epoch_now: i64) -> i64 {
    let key = "\"total_duration_ms\"";
    if let Some(i) = payload.find(key) {
        let rest = &payload[i + key.len()..];
        if let Some(j) = rest.find(':') {
            let digits: String = rest[j + 1..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Some(ms) = short_decimal(&digits) {
                return ms / 1000;
            }
        }
    }
    epoch_now
}

/// Within the throttle window? `last` is the stamp file's text. Pure.
pub fn throttled(last: Option<&str>, now: i64) -> bool {
    let Some(last) = last.and_then(|s| short_decimal(s.trim())) else {
        return false;
    };
    let elapsed = now - last;
    (0..THROTTLE_SECS).contains(&elapsed)
}

/// `application/x-www-form-urlencoded` component encoding (curl's
/// `--data-urlencode`: everything but `A-Za-z0-9-._~` is %-escaped). Pure.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The form body Orca's script sends. Pure.
pub fn form_body(c: &Coords, payload: &str) -> String {
    format!(
        "paneKey={}&configDir={}&env={}&version={}&payload={}",
        urlencode(&c.pane_key),
        urlencode(&c.config_dir),
        urlencode(&c.env),
        urlencode(&c.version),
        urlencode(payload)
    )
}

/// The full HTTP/1.1 request. It carries the token: never log it. Pure.
pub fn request_bytes(c: &Coords, payload: &str) -> Vec<u8> {
    let body = form_body(c, payload);
    let head = format!(
        "POST {STATUSLINE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         X-Orca-Agent-Hook-Token: {token}\r\nContent-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        port = c.port,
        token = c.token.expose(),
        len = body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// The script reads the payload line by line and drops one trailing
/// newline. Pure.
pub fn normalize_payload(raw: &str) -> &str {
    raw.strip_suffix('\n').unwrap_or(raw)
}

// ─── I/O shell ────────────────────────────────────────────────────────────────

fn env_get(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// The endpoint file's entries (empty when unset or unreadable).
fn endpoint_entries(get: &dyn Fn(&str) -> Option<String>) -> HashMap<String, String> {
    get("ORCA_AGENT_HOOK_ENDPOINT")
        .filter(|p| !p.is_empty())
        .and_then(|p| super::read_capped(Path::new(&p), 64 * 1024).ok().flatten())
        .map(|t| parse_endpoint(&t))
        .unwrap_or_default()
}

/// The statusline side: gate, throttle and stamp. `true` when a post is due.
///
/// Under `cfg(test)` this always refuses, so no test can reach a live Orca
/// through the ambient `ORCA_*` env of the shell running `cargo test`; tests
/// drive [`gate_and_stamp_in`] with injected env and temp dir instead.
pub fn gate_and_stamp(payload: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    let windows = cfg!(windows);
    let now = if windows {
        use chrono::Timelike as _;
        i64::from(chrono::Local::now().num_seconds_from_midnight())
    } else {
        posix_now(payload, chrono::Utc::now().timestamp())
    };
    let tmp = stamp_dir(&env_get, windows);
    gate_and_stamp_in(&env_get, &tmp, windows, now, payload)
}

/// Where Orca's script keeps its stamps: `${TMPDIR:-/tmp}` on POSIX (an
/// empty `TMPDIR` counts as unset), `%TEMP%` on Windows. Pure over `get`.
pub fn stamp_dir(get: &dyn Fn(&str) -> Option<String>, windows: bool) -> PathBuf {
    let var = if windows { "TEMP" } else { "TMPDIR" };
    match get(var).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None if windows => std::env::temp_dir(),
        None => PathBuf::from("/tmp"),
    }
}

/// [`gate_and_stamp`] over an injected env, stamp dir and clock.
pub fn gate_and_stamp_in(
    get: &dyn Fn(&str) -> Option<String>,
    tmp: &Path,
    windows: bool,
    now: i64,
    payload: &str,
) -> bool {
    if !payload_wanted(get, payload) {
        return false;
    }
    let Some(c) = coords(get, &endpoint_entries(get)) else {
        return false;
    };
    let path = stamp_path(tmp, &pane_id(&c.pane_key, windows), windows);
    let last = std::fs::read_to_string(&path).ok();
    if throttled(last.as_deref(), now) {
        return false;
    }
    let _ = std::fs::write(&path, now.to_string());
    true
}

/// Start `csm statusline --orca-forward` detached, the payload on its
/// stdin. Best effort: a failure only means Orca misses one reading.
///
/// Under `cfg(test)` it spawns nothing: `current_exe` is the test harness.
pub fn spawn_forward(payload: &str) {
    use std::process::{Command, Stdio};
    if cfg!(test) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("statusline")
        .arg(FORWARD_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
}

/// The forwarding child: read the payload from stdin, re-read the
/// coordinates, POST. Silent whatever happens.
///
/// Under `cfg(test)` it does nothing (the ambient env could name a live Orca).
pub fn run_child() {
    if cfg!(test) {
        return;
    }
    let mut raw = String::new();
    let _ = std::io::stdin().take(PAYLOAD_CAP).read_to_string(&mut raw);
    let payload = normalize_payload(&raw);
    let get = env_get;
    if !payload_wanted(&get, payload) {
        return;
    }
    if let Some(c) = coords(&get, &endpoint_entries(&get)) {
        let _ = post(&c, payload);
    }
}

/// POST to `127.0.0.1:<port>`, bounded by [`CONNECT_TIMEOUT`] and
/// [`TOTAL_TIMEOUT`]. The response is read and discarded. Errors carry only
/// the I/O kind.
pub fn post(c: &Coords, payload: &str) -> std::io::Result<()> {
    let start = Instant::now();
    let addr = SocketAddr::from(([127, 0, 0, 1], c.port));
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    let left = || {
        TOTAL_TIMEOUT
            .checked_sub(start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))
    };
    stream.set_write_timeout(Some(left()?))?;
    stream.write_all(&request_bytes(c, payload))?;
    stream.set_read_timeout(Some(left()?))?;
    let mut sink = [0u8; 1024];
    while start.elapsed() < TOTAL_TIMEOUT {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    const FULL: &[(&str, &str)] = &[
        ("ORCA_AGENT_HOOK_PORT", "4321"),
        ("ORCA_AGENT_HOOK_TOKEN", "tok-env"),
        ("ORCA_AGENT_HOOK_ENV", "production"),
        ("ORCA_AGENT_HOOK_VERSION", "1"),
        ("ORCA_PANE_KEY", "tab-1:3"),
        ("CLAUDE_CONFIG_DIR", "/Users/example/.claude"),
    ];

    #[test]
    fn endpoint_parses_both_shapes_and_strips_cr() {
        let m =
            parse_endpoint("ORCA_AGENT_HOOK_PORT=1\nset ORCA_AGENT_HOOK_TOKEN=t\r\nlower=x\n=y\n");
        assert_eq!(m.get("ORCA_AGENT_HOOK_PORT").map(String::as_str), Some("1"));
        assert_eq!(
            m.get("ORCA_AGENT_HOOK_TOKEN").map(String::as_str),
            Some("t")
        );
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn coords_need_port_token_and_pane_and_prefer_the_file() {
        let get = env(FULL);
        let mut file = HashMap::new();
        file.insert("ORCA_AGENT_HOOK_PORT".to_owned(), "5555".to_owned());
        let c = coords(&get, &file).unwrap();
        assert_eq!(c.port, 5555);
        assert_eq!(c.token.expose(), "tok-env");
        assert_eq!(c.pane_key, "tab-1:3");
        assert!(!format!("{c:?}").contains("tok-env"), "Debug must redact");

        let no_pane = env(&[
            ("ORCA_AGENT_HOOK_PORT", "1"),
            ("ORCA_AGENT_HOOK_TOKEN", "t"),
        ]);
        assert!(coords(&no_pane, &HashMap::new()).is_none());
        let no_token = env(&[("ORCA_AGENT_HOOK_PORT", "1"), ("ORCA_PANE_KEY", "p")]);
        assert!(coords(&no_token, &HashMap::new()).is_none());
    }

    #[test]
    fn payload_gate_matches_the_script() {
        let get = env(&[]);
        assert!(payload_wanted(&get, r#"{"rate_limits":{}}"#));
        assert!(!payload_wanted(&get, r#"{"model":{}}"#));
        assert!(!payload_wanted(&get, ""));
        let job = env(&[("CLAUDE_JOB_DIR", "/tmp/job")]);
        assert!(!payload_wanted(&job, r#"{"rate_limits":{}}"#));
    }

    #[test]
    fn pane_id_derivation() {
        assert_eq!(pane_id("tab-1:3", false), "tab-1_3");
        assert_eq!(pane_id("a:b:12", false), "12", "tab part has a colon");
        assert_eq!(pane_id("tab:x7", false), "x7");
        assert_eq!(
            pane_id("42", false),
            "42_42",
            "a bare key is its own tab part"
        );
        let long = format!("{}:{}", "t".repeat(40), "p".repeat(10));
        let w = pane_id(&long, true);
        assert_eq!(w.chars().count(), 36);
        assert!(!w.contains(':'));
        assert_eq!(pane_id("tab-1:3", true), "tab-1_3");
    }

    #[test]
    fn stamp_dir_follows_the_script() {
        let none = |_: &str| None::<String>;
        let empty = |_: &str| Some(String::new());
        let set = |k: &str| (k == "TMPDIR").then(|| "/var/folders/xy/T/".to_string());
        assert_eq!(stamp_dir(&none, false), PathBuf::from("/tmp"));
        assert_eq!(stamp_dir(&empty, false), PathBuf::from("/tmp"));
        assert_eq!(stamp_dir(&set, false), PathBuf::from("/var/folders/xy/T/"));
        let temp =
            |k: &str| (k == "TEMP").then(|| r"C:\Users\example\AppData\Local\Temp".to_string());
        assert_eq!(
            stamp_dir(&temp, true),
            PathBuf::from(r"C:\Users\example\AppData\Local\Temp")
        );
    }

    #[test]
    fn stamp_paths() {
        let t = Path::new("/tmp");
        assert_eq!(
            stamp_path(t, "x_1", false),
            Path::new("/tmp/orca-claude-statusline-last-x_1")
        );
        assert_eq!(
            stamp_path(t, "x_1", true),
            Path::new("/tmp/orca-claude-statusline-last-x_1.tmp")
        );
    }

    #[test]
    fn clock_prefers_the_payload_duration() {
        assert_eq!(posix_now(r#"{"cost":{"total_duration_ms": 65432}}"#, 9), 65);
        assert_eq!(posix_now(r#"{"cost":{"total_duration_ms":"x"}}"#, 9), 9);
        assert_eq!(posix_now(r#"{"total_duration_ms":1234567890123456}"#, 9), 9);
        assert_eq!(posix_now("{}", 9), 9);
    }

    #[test]
    fn throttle_window() {
        assert!(throttled(Some("100"), 100));
        assert!(throttled(Some("100"), 114));
        assert!(!throttled(Some("100"), 115));
        assert!(!throttled(Some("100"), 99), "clock went back");
        assert!(!throttled(Some("abc"), 100));
        assert!(!throttled(Some("0100"), 100), "non-canonical");
        assert!(!throttled(None, 100));
    }

    #[test]
    fn form_body_and_request() {
        let c = coords(&env(FULL), &HashMap::new()).unwrap();
        let body = form_body(&c, r#"{"a":1}"#);
        assert_eq!(
            body,
            "paneKey=tab-1%3A3&configDir=%2FUsers%2Fexample%2F.claude&env=production&version=1&payload=%7B%22a%22%3A1%7D"
        );
        let req = String::from_utf8(request_bytes(&c, r#"{"a":1}"#)).unwrap();
        assert!(req.starts_with("POST /statusline/claude HTTP/1.1\r\n"));
        assert!(req.contains("X-Orca-Agent-Hook-Token: tok-env\r\n"));
        assert!(req.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(req.ends_with(&body));
    }

    #[test]
    fn payload_loses_one_trailing_newline() {
        assert_eq!(normalize_payload("{}\n"), "{}");
        assert_eq!(normalize_payload("{}"), "{}");
        assert_eq!(normalize_payload("{}\n\n"), "{}\n");
    }

    /// The POST against a throwaway loopback listener (never Orca's).
    #[test]
    fn post_sends_the_request_to_the_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !String::from_utf8_lossy(&buf).contains("payload=") {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            String::from_utf8_lossy(&buf).into_owned()
        });
        let mut c = coords(&env(FULL), &HashMap::new()).unwrap();
        c.port = port;
        post(&c, r#"{"rate_limits":{}}"#).unwrap();
        let got = server.join().unwrap();
        assert!(got.starts_with("POST /statusline/claude"), "{got}");
        assert!(got.contains("payload=%7B%22rate_limits%22%3A%7B%7D%7D"));
    }

    #[test]
    fn gate_stamps_once_then_throttles() {
        let tmp = tempfile::tempdir().unwrap();
        let get = env(FULL);
        let payload = r#"{"rate_limits":{}}"#;
        assert!(gate_and_stamp_in(&get, tmp.path(), false, 1_000, payload));
        let stamp = stamp_path(tmp.path(), &pane_id("tab-1:3", false), false);
        assert_eq!(std::fs::read_to_string(&stamp).unwrap(), "1000");
        assert!(!gate_and_stamp_in(&get, tmp.path(), false, 1_010, payload));
        assert!(gate_and_stamp_in(&get, tmp.path(), false, 1_015, payload));
        // No rate_limits, or no Orca coordinates: nothing is due or stamped.
        assert!(!gate_and_stamp_in(&get, tmp.path(), false, 2_000, "{}"));
        let none = env(&[]);
        let other = tempfile::tempdir().unwrap();
        assert!(!gate_and_stamp_in(
            &none,
            other.path(),
            false,
            2_000,
            payload
        ));
        assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 0);
    }

    #[test]
    fn io_entry_points_refuse_under_test() {
        // Even with a full Orca env present, the ambient entry points do
        // nothing in a test build: no stamp, no child, no connection.
        crate::testenv::with_env_vars(
            &[
                ("ORCA_AGENT_HOOK_PORT", Some("1")),
                ("ORCA_AGENT_HOOK_TOKEN", Some("tok")),
                ("ORCA_PANE_KEY", Some("tab-guard:1")),
                ("ORCA_AGENT_HOOK_ENDPOINT", None),
                ("CLAUDE_JOB_DIR", None),
            ],
            || {
                assert!(!gate_and_stamp(r#"{"rate_limits":{}}"#));
                spawn_forward(r#"{"rate_limits":{}}"#);
                run_child();
            },
        );
    }
}
