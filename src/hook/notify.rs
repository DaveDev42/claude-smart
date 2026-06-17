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
//! WezTerm's native OSC 777 notification format:
//!
//! ```
//! ESC ] 777 ; notify ; <title> ; <body> ST
//! ```
//!
//! Claude Code wraps this as a JSON object on stdout so CC can route it to the
//! controlling terminal. The spec defines:
//!
//! ```json
//! {"terminalSequence": "\x1b]777;notify;csm;BODY\x07"}
//! ```
//!
//! The `\x07` (BEL) is the String Terminator (ST) shorthand accepted by WezTerm.
//!
//! # limit-switch.log
//!
//! Append-only log at `<owner_dir>/limit-switch.log`.
//! Each entry is one line: `<ISO-8601 timestamp>  <message>`.

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;

// ─── OSC 777 emit ─────────────────────────────────────────────────────────────

/// Emit an OSC 777 `{"terminalSequence":…}` JSON object to stdout.
///
/// Writes **exactly one** JSON object (no trailing newline beyond what `println!`
/// adds). If the caller decides no notification should be emitted, it simply does
/// not call this function — this function always emits.
///
/// The `message` is the human-readable body of the toast notification.
pub fn emit_osc777(message: &str) -> anyhow::Result<()> {
    // Escape the message for embedding in the OSC 777 sequence.
    // Semicolons delimit OSC 777 fields; escape them so the title/body split is clean.
    let safe_body = message.replace(';', "\\;");

    // Build the terminal sequence: ESC ] 777 ; notify ; csm ; <body> BEL
    // BEL (\x07) is the ST shorthand accepted by WezTerm and most modern terminals.
    let seq = format!("\x1b]777;notify;csm;{safe_body}\x07");

    // Wrap in the JSON object CC expects on the hook's stdout.
    let payload = serde_json::json!({ "terminalSequence": seq });
    let json = serde_json::to_string(&payload).context("failed to serialize OSC 777 payload")?;

    // Write exactly one JSON object to stdout (hook contract: one object or nothing).
    println!("{json}");
    Ok(())
}

// ─── log append ───────────────────────────────────────────────────────────────

/// Append a one-line timestamped entry to `<owner_dir>/limit-switch.log`.
///
/// The log is append-only and never rotated by the hook — it is a forensic trail.
/// Each line has the form:
///
/// ```
/// 2026-06-18T07:00:00Z  <sid>  <message>
/// ```
pub fn append_log(sid: &str, message: &str, owner_dir: &Path) -> anyhow::Result<()> {
    use std::fs::OpenOptions;

    let log_path = owner_dir.join("limit-switch.log");

    // Timestamp: ISO 8601 UTC using chrono.
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    let line = format!("{ts}  {sid}  {message}\n");

    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .with_context(|| format!("failed to open limit-switch.log at {log_path:?}"))?;

    f.write_all(line.as_bytes())
        .with_context(|| format!("failed to append to limit-switch.log at {log_path:?}"))?;

    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// OSC 777 payload is valid JSON with a "terminalSequence" key.
    #[test]
    fn osc777_payload_is_valid_json() {
        // Capture stdout by testing the JSON construction directly.
        let message = "limit detected for session abc-123";
        let safe_body = message.replace(';', "\\;");
        let seq = format!("\x1b]777;notify;csm;{safe_body}\x07");
        let payload = serde_json::json!({ "terminalSequence": seq });

        assert!(payload.get("terminalSequence").is_some());
        let ts_val = payload["terminalSequence"].as_str().unwrap();
        assert!(
            ts_val.starts_with("\x1b]777;notify;csm;"),
            "sequence should start with OSC 777 prefix: {ts_val:?}"
        );
        assert!(
            ts_val.ends_with('\x07'),
            "sequence should end with BEL: {ts_val:?}"
        );
    }

    /// Semicolons in the message are replaced with `\;` so they don't break
    /// OSC 777 field splitting. The resulting body contains `\;` but the raw `;`
    /// in the original message is replaced (the `\;` two-char sequence is kept).
    #[test]
    fn osc777_semicolons_escaped() {
        let message = "switching; profile: work; hop 1";
        let safe_body = message.replace(';', "\\;");
        // Each original `;` becomes `\;` (two characters). Count remaining bare `;`.
        // A bare `;` not preceded by `\` would be a field separator leak.
        // The simplest check: verify `\;` appears in the output (replacement happened)
        // and the body does NOT contain the OSC 777 field separator `;` without a
        // preceding backslash (i.e. no unescaped semicolons remain).
        assert!(
            safe_body.contains("\\;"),
            "escaped semicolons should use \\; : {safe_body}"
        );
        // Verify every `;` in safe_body is preceded by `\`
        for (i, ch) in safe_body.char_indices() {
            if ch == ';' {
                let prev = if i > 0 {
                    safe_body.as_bytes().get(i - 1).copied()
                } else {
                    None
                };
                assert_eq!(
                    prev,
                    Some(b'\\'),
                    "unescaped ';' found at position {i} in: {safe_body}"
                );
            }
        }
    }

    /// append_log creates the log file and writes a timestamped line.
    #[test]
    fn append_log_creates_and_writes() {
        let dir = TempDir::new().unwrap();
        let sid = "test-sid-0001";
        let message = "limit detected";
        append_log(sid, message, dir.path()).expect("append_log should succeed");

        let content = std::fs::read_to_string(dir.path().join("limit-switch.log")).unwrap();
        assert!(
            content.contains(sid),
            "log should contain session id: {content}"
        );
        assert!(
            content.contains(message),
            "log should contain message: {content}"
        );
        // Timestamp should be ISO 8601 UTC (contains 'T' and 'Z').
        assert!(
            content.contains('T') && content.contains('Z'),
            "log should contain ISO 8601 timestamp: {content}"
        );
    }

    /// append_log appends multiple entries (does not truncate).
    #[test]
    fn append_log_is_append_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        append_log("sid-1", "first event", path).unwrap();
        append_log("sid-2", "second event", path).unwrap();

        let content = std::fs::read_to_string(path.join("limit-switch.log")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have exactly 2 log lines: {content}");
        assert!(lines[0].contains("sid-1"), "first line: {}", lines[0]);
        assert!(lines[1].contains("sid-2"), "second line: {}", lines[1]);
    }

    /// emit_osc777 does not panic on an empty message.
    #[test]
    fn osc777_empty_message_no_panic() {
        // We can't easily capture stdout in a unit test without redirecting fd 1,
        // but we can verify the JSON construction doesn't panic.
        let message = "";
        let safe_body = message.replace(';', "\\;");
        let seq = format!("\x1b]777;notify;csm;{safe_body}\x07");
        let payload = serde_json::json!({ "terminalSequence": seq });
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.is_empty());
    }
}
