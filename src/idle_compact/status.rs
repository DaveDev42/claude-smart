//! Session status veto: `<config dir>/sessions/<claude pid>.json`, Claude
//! Code's own internal per-process record. This is a small, purpose-built
//! reader for exactly the two fields the veto needs — not
//! `orca::runtime`'s session-liveness registry parser, which reads the same
//! directory for a different purpose (Orca's own refresh gating) and does
//! not track `waitingFor` at all; keeping this independent avoids coupling
//! idle-compact's veto logic to that module's session-liveness semantics.
//!
//! A readable file with `status != "idle"` or a non-empty `waitingFor`
//! vetoes typing; a missing or unreadable file never does — "no signal" is
//! not "safe to type" on its own, but every caller here is already gated by
//! every other check in [`super::deliver`]'s state machine before this one
//! runs (see the design spec's "Typing protocol" step 2).

#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The subset of Claude Code's session-registry file this module reads.
/// Every other field (pid, sessionId, cwd, …) is ignored — kept loose
/// (`Option` + `#[serde(default)]`) so a shape this crate does not expect
/// never turns "unrelated field changed" into "must be a veto".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SessionStatus {
    /// `"idle"`/`"busy"`/`"waiting"`/`"shell"` (Claude Code's own words;
    /// an unrecognised value is treated the same as any other non-`"idle"`
    /// value by [`veto_reason`] — a future word claude adds must veto, not
    /// be silently ignored).
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "waitingFor")]
    pub waiting_for: Option<String>,
}

/// `<config_dir>/sessions/<claude_pid>.json`.
pub fn session_status_path(config_dir: &Path, claude_pid: u32) -> PathBuf {
    config_dir
        .join("sessions")
        .join(format!("{claude_pid}.json"))
}

/// Parse `bytes` as a [`SessionStatus`]. `None` on anything unparseable —
/// treated by [`check`] exactly like a missing file.
pub fn parse_session_status(bytes: &[u8]) -> Option<SessionStatus> {
    serde_json::from_slice(bytes).ok()
}

/// Read and parse `path`, following symlinks (`std::fs::read`'s default
/// behavior). `None` on any I/O or parse error.
pub fn read_status(path: &Path) -> Option<SessionStatus> {
    let bytes = std::fs::read(path).ok()?;
    parse_session_status(&bytes)
}

/// The pure veto decision: `Some(reason)` when typing must not proceed,
/// `None` when this check has nothing against it. `reason` is `status`'s
/// own value when it is present, non-blank and not `"idle"` (safe to log —
/// these are Claude Code's own short status words, never free text);
/// otherwise `"waiting"` when `status` is `"idle"`/absent but `waitingFor`
/// is a non-blank string (never logging `waitingFor`'s own text, which is
/// free-form and could be private).
pub fn veto_reason(status: &SessionStatus) -> Option<String> {
    if let Some(s) = status.status.as_deref().map(str::trim)
        && !s.is_empty()
        && s != "idle"
    {
        return Some(s.to_owned());
    }
    let waiting_for_is_set = status
        .waiting_for
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    waiting_for_is_set.then(|| "waiting".to_owned())
}

/// Read `<config_dir>/sessions/<claude_pid>.json` and return a veto reason,
/// if any. Missing or unreadable file (including unparseable JSON): `None`
/// — no veto, matching the design spec's "Missing or unreadable means no
/// veto".
pub fn check(config_dir: &Path, claude_pid: u32) -> Option<String> {
    let status = read_status(&session_status_path(config_dir, claude_pid))?;
    veto_reason(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn veto_reason_none_when_idle_and_no_waiting_for() {
        let s = SessionStatus {
            status: Some("idle".to_owned()),
            waiting_for: None,
        };
        assert_eq!(veto_reason(&s), None);
    }

    #[test]
    fn veto_reason_none_when_status_absent_and_no_waiting_for() {
        assert_eq!(veto_reason(&SessionStatus::default()), None);
    }

    #[test]
    fn veto_reason_is_the_status_word_when_not_idle() {
        for status in ["busy", "waiting", "shell"] {
            let s = SessionStatus {
                status: Some(status.to_owned()),
                waiting_for: None,
            };
            assert_eq!(veto_reason(&s), Some(status.to_owned()), "status={status}");
        }
    }

    #[test]
    fn veto_reason_is_waiting_when_idle_but_waiting_for_is_set() {
        let s = SessionStatus {
            status: Some("idle".to_owned()),
            waiting_for: Some("permission".to_owned()),
        };
        assert_eq!(veto_reason(&s), Some("waiting".to_owned()));
    }

    #[test]
    fn veto_reason_is_waiting_when_status_absent_but_waiting_for_is_set() {
        let s = SessionStatus {
            status: None,
            waiting_for: Some("permission".to_owned()),
        };
        assert_eq!(veto_reason(&s), Some("waiting".to_owned()));
    }

    #[test]
    fn veto_reason_blank_waiting_for_does_not_veto() {
        let s = SessionStatus {
            status: Some("idle".to_owned()),
            waiting_for: Some("   ".to_owned()),
        };
        assert_eq!(veto_reason(&s), None);
    }

    #[test]
    fn parse_session_status_reads_status_and_camelcase_waiting_for() {
        let s = parse_session_status(br#"{"status":"busy","waitingFor":"tool"}"#).unwrap();
        assert_eq!(s.status.as_deref(), Some("busy"));
        assert_eq!(s.waiting_for.as_deref(), Some("tool"));
    }

    #[test]
    fn parse_session_status_null_waiting_for_is_none() {
        let s = parse_session_status(br#"{"status":"idle","waitingFor":null}"#).unwrap();
        assert_eq!(s.waiting_for, None);
    }

    #[test]
    fn parse_session_status_ignores_unknown_fields() {
        let s = parse_session_status(br#"{"status":"idle","pid":123,"cwd":"/x"}"#).unwrap();
        assert_eq!(s.status.as_deref(), Some("idle"));
    }

    #[test]
    fn parse_session_status_none_for_unparseable_bytes() {
        assert!(parse_session_status(b"not json").is_none());
    }

    #[test]
    fn session_status_path_joins_sessions_and_pid() {
        let p = session_status_path(Path::new("/cfg"), 4242);
        assert_eq!(p, Path::new("/cfg/sessions/4242.json"));
    }

    #[test]
    fn read_status_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_status(&tmp.path().join("nope.json")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_status_follows_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.json");
        std::fs::write(&real, br#"{"status":"busy"}"#).unwrap();
        let link = tmp.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let s = read_status(&link).expect("must follow the symlink");
        assert_eq!(s.status.as_deref(), Some("busy"));
    }

    #[test]
    fn check_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check(tmp.path(), 4242).is_none());
    }

    #[test]
    fn check_vetoes_when_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("4242.json"), br#"{"status":"busy"}"#).unwrap();
        assert_eq!(check(tmp.path(), 4242), Some("busy".to_owned()));
    }

    #[test]
    fn check_none_when_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("4242.json"), br#"{"status":"idle"}"#).unwrap();
        assert_eq!(check(tmp.path(), 4242), None);
    }
}
