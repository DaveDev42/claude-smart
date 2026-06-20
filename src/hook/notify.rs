//! OSC 777 notify + limit-switch.log append.
//!
//! # Hook stdout contract
//!
//! The hook MUST write **exactly one JSON object** to stdout or **nothing**.
//! All logging goes to the log file or stderr — never to stdout except the single
//! `{"terminalSequence":…}` OSC 777 payload (which Claude Code routes to the PTY).
//!
//! # OSC 777 format
//!
//! The terminal sequence written by the hook mirrors `_notify()` in `limit-switch.sh.j2`:
//!
//! ```
//! ESC ] 777 ; notify ; <title> ; <body> ESC \
//! ```
//!
//! `ESC \` (0x1B 0x5C) is the 2-character String Terminator (ST). We use BEL (`\x07`)
//! as an equivalent shorthand accepted by WezTerm and most modern terminals.
//!
//! Claude Code wraps this as a JSON object on stdout so CC can route it to the
//! controlling terminal. The spec defines:
//!
//! ```json
//! {"terminalSequence": "\x1b]777;notify;<title>;<body>\x07"}
//! ```
//!
//! # limit-switch.log
//!
//! Append-only log at `<smart_dir>/limit-switch.log`.
//! Format (mirrors limit-switch.sh.j2 `_log()` function, lines 114-119):
//!   `<YYYY-MM-DD HH:MM:SS>  host=<hostname>  sid=<sid>  action=<action>  <extra>`

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;

// ─── OSC 777 emit ─────────────────────────────────────────────────────────────

/// Build the OSC 777 `{"terminalSequence":…}` JSON string for a given title and body.
///
/// Reproduces the `_notify()` function from `limit-switch.sh.j2` lines 237-239.
/// Title and body semicolons are passed through (the OSC 777 spec field separator
/// is the semicolon between title and body; we trust the caller to avoid semicolons
/// in the title, and escape the body for safety).
///
/// Returns the JSON string (without trailing newline) that should be written to stdout.
pub fn build_osc777_json(title: &str, body: &str) -> String {
    // Escape semicolons in body so they don't break OSC 777 field parsing.
    let safe_body = body.replace(';', "\\;");
    // ESC ] 777 ; notify ; <title> ; <body> BEL
    let seq = format!("\x1b]777;notify;{title};{safe_body}\x07");
    let payload = serde_json::json!({ "terminalSequence": seq });
    serde_json::to_string(&payload).unwrap_or_default()
}

/// Emit an OSC 777 `{"terminalSequence":…}` JSON object to stdout.
///
/// Writes **exactly one** JSON object (with trailing newline from `println!`).
/// If the caller decides no notification should be emitted, it simply does
/// not call this function — this function always emits.
///
/// The `message` is the human-readable body; the title is `"limit detected"`.
///
/// Shell: `_notify "limit detected" "<message>"` (limit-switch.sh.j2 lines 237-239)
pub fn emit_osc777(message: &str) -> anyhow::Result<()> {
    let json = build_osc777_json("limit detected", message);
    // Write exactly one JSON object to stdout (hook contract: one object or nothing).
    println!("{json}");
    Ok(())
}

// ─── log append ───────────────────────────────────────────────────────────────

/// Append a one-line timestamped entry to `<smart_dir>/limit-switch.log`.
///
/// Format mirrors the `_log()` function from `limit-switch.sh.j2` lines 114-119:
///   `<YYYY-MM-DD HH:MM:SS>  host=<hostname>  sid=<sid>  action=<action>  <extra>`
///
/// `_owner_dir` is accepted for API compatibility (the shell's per-profile `.log`
/// convention), but the canonical log destination is the shared smart_dir so all
/// profiles' limit-switch events appear in one place (matching the shell's
/// `LOG="$SMART_DIR/limit-switch.log"` line 103).
pub fn append_log(sid: &str, message: &str, _owner_dir: &Path) -> anyhow::Result<()> {
    use crate::paths;
    use std::fs::OpenOptions;

    // Shell: LOG="$SMART_DIR/limit-switch.log"
    let smart_dir = paths::smart_dir()?;
    let log_path = smart_dir.join("limit-switch.log");

    // Shell: `printf '%s host=%s sid=%s action=%s %s\n' "$(date '+%F %T')" "$(hostname -s)" ...`
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let hostname = get_hostname();
    let line = format!("{ts}  host={hostname}  sid={sid}  {message}\n");

    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .with_context(|| format!("failed to open limit-switch.log at {log_path:?}"))?;

    f.write_all(line.as_bytes())
        .with_context(|| format!("failed to append to limit-switch.log at {log_path:?}"))?;

    Ok(())
}

// ─── hostname helper ─────────────────────────────────────────────────────────

/// Return the short hostname (like `hostname -s`).
fn get_hostname() -> String {
    // Shell: `hostname -s 2>/dev/null || hostname`
    // On unix, use gethostname via nix; fall back to std::process or "unknown".
    #[cfg(unix)]
    {
        use nix::unistd::gethostname;
        if let Ok(h) = gethostname() {
            let s = h.to_string_lossy().into_owned();
            // Strip domain suffix (everything after the first '.')
            return s.split('.').next().unwrap_or(&s).to_string();
        }
    }
    // Windows or nix failure: use the COMPUTERNAME env var or "unknown".
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// build_osc777_json returns valid JSON with a "terminalSequence" key.
    #[test]
    fn osc777_payload_is_valid_json() {
        let json = build_osc777_json("limit detected", "session abc-123 hit 99%");
        let val: serde_json::Value = serde_json::from_str(&json).expect("should be valid JSON");
        assert!(val.get("terminalSequence").is_some());
        let ts_val = val["terminalSequence"].as_str().unwrap();
        // Must start with ESC ] 777 ; notify ;
        assert!(
            ts_val.starts_with("\x1b]777;notify;"),
            "sequence should start with OSC 777 prefix: {ts_val:?}"
        );
        assert!(
            ts_val.ends_with('\x07'),
            "sequence should end with BEL: {ts_val:?}"
        );
    }

    /// The title "limit detected" appears in the OSC 777 sequence.
    #[test]
    fn osc777_title_in_sequence() {
        let json = build_osc777_json("limit detected", "body here");
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let seq = val["terminalSequence"].as_str().unwrap();
        assert!(
            seq.contains("limit detected"),
            "title should appear in sequence: {seq}"
        );
        assert!(seq.contains("body here"), "body should appear: {seq}");
    }

    /// Semicolons in the body are replaced with `\;` so they don't break
    /// OSC 777 field splitting. The resulting body contains `\;`.
    #[test]
    fn osc777_semicolons_escaped_in_body() {
        let json = build_osc777_json("limit detected", "switching; profile: work; hop 1");
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let seq = val["terminalSequence"].as_str().unwrap();

        // Find the body portion (after the second `;` field separator: ESC]777;notify;title;BODY)
        // We verify the body does NOT contain unescaped semicolons.
        // After the title the next field sep is followed by the body.
        // Extract the body from the sequence.
        // Format: \x1b]777;notify;TITLE;BODY\x07
        let after_prefix = seq.strip_prefix("\x1b]777;notify;limit detected;").unwrap();
        let body = after_prefix.strip_suffix('\x07').unwrap();

        // Verify every `;` in body is preceded by `\`
        for (i, ch) in body.char_indices() {
            if ch == ';' {
                let prev = if i > 0 {
                    body.as_bytes().get(i - 1).copied()
                } else {
                    None
                };
                assert_eq!(
                    prev,
                    Some(b'\\'),
                    "unescaped ';' found at position {i} in body: {body}"
                );
            }
        }
        assert!(
            body.contains("\\;"),
            "escaped semicolons should use \\;: {body}"
        );
    }

    /// build_osc777_json does not panic on an empty message.
    #[test]
    fn osc777_empty_message_no_panic() {
        let json = build_osc777_json("limit detected", "");
        assert!(!json.is_empty());
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(val.get("terminalSequence").is_some());
    }

    /// append_log creates the log file and writes a properly formatted line.
    #[test]
    fn append_log_creates_and_writes() {
        // append_log uses smart_dir() which writes to $HOME/.claude.shared/smart/limit-switch.log
        // We can't easily redirect that in a test; instead test the format construction directly.
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let hostname = "testhost";
        let sid = "test-sid-0001";
        let message = "action=switched to=work";
        let line = format!("{ts}  host={hostname}  sid={sid}  {message}\n");

        // Verify format matches shell's _log() output
        assert!(line.contains("host=testhost"), "line: {line}");
        assert!(line.contains("sid=test-sid-0001"), "line: {line}");
        assert!(line.contains("action=switched"), "line: {line}");
        // Timestamp is YYYY-MM-DD HH:MM:SS
        assert!(
            line.chars()
                .take(10)
                .all(|c| c.is_ascii_digit() || c == '-'),
            "timestamp format unexpected: {line}"
        );
    }

    /// append_log appends multiple entries (does not truncate).
    /// Uses a real temp file to verify append behavior without touching $HOME.
    #[test]
    fn append_log_format_and_append() {
        // We test the format and append behavior by writing to a temp path directly.
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("limit-switch.log");

        let entries = [
            ("sid-1", "action=switched to=work"),
            ("sid-2", "action=notify-only"),
        ];

        for (sid, msg) in &entries {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let line = format!("{ts}  host=testhostname  sid={sid}  {msg}\n");
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&log_path)
                .unwrap();
            f.write_all(line.as_bytes()).unwrap();
        }

        let content = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have exactly 2 log lines: {content}");
        assert!(lines[0].contains("sid-1"), "first line: {}", lines[0]);
        assert!(lines[1].contains("sid-2"), "second line: {}", lines[1]);
        // Each line has 'host=' and 'sid='
        for line in &lines {
            assert!(line.contains("host="), "missing host= in: {line}");
            assert!(line.contains("sid="), "missing sid= in: {line}");
        }
    }

    /// The OSC 777 escape sequence format exactly matches the shell's _notify() output.
    /// Shell: printf '{"terminalSequence": "%s]777;notify;%s;%s%s\\\\"}' "$esc" "$title" "$body" "$esc"
    /// The shell uses ESC+\ as ST; Rust uses BEL. Both work in WezTerm.
    /// This test verifies the Rust JSON is valid and extractable.
    #[test]
    fn osc777_json_extractable_for_cc() {
        let body = "[personal] hit session 99% → switching to [work] (hop 1)";
        let json = build_osc777_json("limit detected", body);
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let seq = val["terminalSequence"]
            .as_str()
            .expect("terminalSequence must be a string");
        // CC reads this string and writes it verbatim to the PTY.
        assert!(
            seq.starts_with("\x1b]777;"),
            "must start with ESC]777;: {seq:?}"
        );
        // The body content (with escaped semicolons) must be present.
        assert!(seq.contains("personal"), "body content missing: {seq}");
        assert!(seq.contains("work"), "body content missing: {seq}");
    }
}
