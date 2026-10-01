//! The one-line-per-outcome log format (`<smart_dir>/idle-compact.log`),
//! shared by [`super::tick`]'s own two outcomes
//! (`handed-off`/`no-delivery-path`) and the future pty-relay supervisor's
//! delivery outcomes (`delivered`/`sent-unconfirmed`/`draft`/
//! `verify-failed`/`expired`/`vetoed-<reason>`/the `dry-run-*` variants —
//! see [`super::deliver::Outcome`]). Mirrors `hook::notify::append_log`'s
//! line format (`<ts>  host=<h>  sid=<sid>  <message>`); duplicated locally
//! (rather than widening that function to take a log filename) because
//! that function's destination is hardcoded to `limit-switch.log` for a
//! different feature.

use crate::paths;

/// The known-when fields a [`log_outcome`] caller has in hand. Every field
/// but the `outcome` word itself (passed separately) is optional: `tick`
/// knows `remaining_secs`/`recache_tokens`/`context_window` but nothing
/// about the screen or vim; the supervisor knows all of them once it has
/// read a screen and a session status.
#[derive(Debug, Clone, Default)]
pub struct LogFields<'a> {
    pub remaining_secs: Option<i64>,
    pub recache_tokens: Option<i64>,
    pub context_window: Option<&'a str>,
    /// The input-box classification (`empty`/`empty-vim-normal`/
    /// `empty-vim-insert`/`draft`/`not-found`), when the supervisor has
    /// checked the screen.
    pub box_state: Option<&'a str>,
    /// The vim mode observed, when known.
    pub vim: Option<&'a str>,
    /// The session-status veto reason, when a status file was read and
    /// vetoed (`busy`/`waiting`/`shell`/...).
    pub status: Option<&'a str>,
    /// The input box's text at verify time (verify-failed only), with
    /// whitespace collapsed and quoted by `{:?}`.
    pub box_text: Option<&'a str>,
    /// The slash menu at verify time: `none` or `<entries>:<highlighted|none>`
    /// (verify-failed only).
    pub menu: Option<&'a str>,
}

/// Append one `outcome=<outcome> ...` line to [`paths::idle_compact_log`].
/// Best-effort — a logging failure must never fail the caller (`tick`'s own
/// I/O shell, or the supervisor's delivery loop).
pub fn log_outcome(sid: &str, outcome: &str, fields: &LogFields) {
    let mut message = format!("outcome={outcome}");
    if let Some(v) = fields.remaining_secs {
        message.push_str(&format!(" remaining_secs={v}"));
    }
    if let Some(v) = fields.recache_tokens {
        message.push_str(&format!(" recache_tokens={v}"));
    }
    if let Some(v) = fields.context_window {
        message.push_str(&format!(" context_window={v}"));
    }
    if let Some(v) = fields.box_state {
        message.push_str(&format!(" box={v}"));
    }
    if let Some(v) = fields.vim {
        message.push_str(&format!(" vim={v}"));
    }
    if let Some(v) = fields.status {
        message.push_str(&format!(" status={v}"));
    }
    if let Some(v) = fields.box_text {
        message.push_str(&format!(" box_text={v:?}"));
    }
    if let Some(v) = fields.menu {
        message.push_str(&format!(" menu={v}"));
    }
    append_line(sid, &message);
}

/// Append one already-built `message` line, timestamped and hostnamed the
/// same way every other line in this log is. Best-effort.
fn append_line(sid: &str, message: &str) {
    use std::io::Write as _;

    let log_path = paths::idle_compact_log();
    let Some(dir) = log_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let hostname = crate::hook::notify::get_hostname();
    let line = format!("{ts}  host={hostname}  sid={sid}  {message}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_outcome_writes_only_the_known_fields() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            log_outcome(
                "sid-1",
                "handed-off",
                &LogFields {
                    remaining_secs: Some(120),
                    recache_tokens: Some(150_000),
                    ..Default::default()
                },
            );
            let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
            assert!(log.contains("outcome=handed-off"), "log: {log}");
            assert!(log.contains("remaining_secs=120"), "log: {log}");
            assert!(log.contains("recache_tokens=150000"), "log: {log}");
            assert!(log.contains("sid=sid-1"), "log: {log}");
            assert!(!log.contains("box="), "log: {log}");
            assert!(!log.contains("vim="), "log: {log}");
            assert!(!log.contains("status="), "log: {log}");
        });
    }

    #[test]
    fn log_outcome_includes_box_vim_status_when_known() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            log_outcome(
                "sid-2",
                "delivered",
                &LogFields {
                    box_state: Some("empty"),
                    vim: Some("insert"),
                    status: Some("busy"),
                    ..Default::default()
                },
            );
            let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
            assert!(log.contains("box=empty"), "log: {log}");
            assert!(log.contains("vim=insert"), "log: {log}");
            assert!(log.contains("status=busy"), "log: {log}");
        });
    }

    #[test]
    fn log_outcome_appends_multiple_lines() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            log_outcome("sid-3", "no-delivery-path", &LogFields::default());
            log_outcome("sid-3", "no-delivery-path", &LogFields::default());
            let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
            assert_eq!(log.lines().count(), 2, "log: {log}");
        });
    }
}
