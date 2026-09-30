//! Hand-off request files: `<smart_dir>/idle-compact-requests/<supervisor
//! pid>.json`, written by [`super::tick`]'s I/O shell and consumed by
//! csm's own pty-relay supervisor (not built here — see the design spec's
//! "Request hand-off" section). The request never carries text to type;
//! the text is the constant `/compact`, decided by the supervisor's
//! [`super::deliver`] state machine.
//!
//! Every function here takes an explicit `dir: &Path` (mirroring
//! `crate::config::Config::load_from`'s explicit-path seam) so a test can
//! point at a temp dir without going through
//! [`crate::paths::idle_compact_requests_dir`]. The real call site uses
//! that constructor; the functions below each join `<pid>.json` onto `dir`
//! themselves rather than taking a pre-built file path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::orca::fsx::{self, WriteOpts};

/// The request schema version this binary writes. [`take_request`] and
/// [`prune_stale_requests`] drop any file whose `v` is not this value, or
/// that has no parseable `v` at all — unlike the relaunch sentinel's
/// read-compat defaulting, an unrecognised or missing version here is
/// treated as malformed rather than silently assumed current.
pub const REQUEST_V: u32 = 1;

/// One idle-compact hand-off: `tick` decided this session's cache should
/// be compacted and a supervisor is alive to act on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    /// `"on"` or `"dry-run"` ([`crate::config::IdleCompactMode::as_str`]).
    pub mode: String,
    pub sid: String,
    /// Epoch seconds `tick` wrote this.
    pub written_at: i64,
    /// Epoch seconds past which the supervisor must stop retrying
    /// (`expires_at - 20`, per the design spec).
    pub deadline: i64,
    pub recache_tokens: i64,
    pub remaining_secs: i64,
    /// The statusline payload's `vim.mode`, when present.
    #[serde(default)]
    pub vim_mode: Option<String>,
}

/// `<dir>/<pid>.json`.
fn path_for(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// Write `req` atomically to `<dir>/<supervisor_pid>.json`, private
/// permissions, parent dir created 0700 — the same `orca::fsx` helpers the
/// relaunch sentinel uses.
pub fn write_request(dir: &Path, supervisor_pid: u32, req: &Request) -> std::io::Result<()> {
    let path = path_for(dir, supervisor_pid);
    if let Some(parent) = path.parent() {
        fsx::create_dir_all(parent, 0o700)?;
    }
    let json = serde_json::to_vec(req)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fsx::write_atomic(&path, &json, WriteOpts::PRIVATE)
}

/// `true` when `req` is still usable at `now`: a known schema version and a
/// deadline that has not yet passed (`deadline < now` is expired;
/// `deadline == now` still counts, matching `tick::decide`'s own
/// not-yet-expired convention of strict-greater-than for "too late").
fn is_live(req: &Request, now: i64) -> bool {
    req.v == REQUEST_V && req.deadline >= now
}

/// Read then remove `<dir>/<supervisor_pid>.json`. `None` when the file is
/// absent, unreadable, unparseable, a schema version this binary does not
/// know ([`REQUEST_V`]), or already past its `deadline` at `now` — every
/// one of those is treated as "nothing to act on", and the file is removed
/// regardless (a malformed or expired request left in place would only be
/// re-read and re-rejected on the supervisor's next poll).
// Supervisor-side reader: the pty-relay supervisor that calls this is a
// separate module, not yet built in this crate.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn take_request(dir: &Path, supervisor_pid: u32, now: i64) -> Option<Request> {
    let path = path_for(dir, supervisor_pid);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let req: Request = serde_json::from_slice(&bytes).ok()?;
    is_live(&req, now).then_some(req)
}

/// Remove a leftover request left under this supervisor's OWN pid, called
/// once at supervisor start (a pid can be reused by the OS, and a request
/// meant for a prior process running as the same pid must never be acted
/// on by a new one). Best-effort.
// Called by the supervisor at start; the supervisor is a separate module,
// not yet built in this crate.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn clear_own_request(dir: &Path, pid: u32) {
    let _ = std::fs::remove_file(path_for(dir, pid));
}

/// Sweep every `<dir>/*.json` file and remove the ones [`take_request`]
/// would have dropped anyway (malformed, wrong `v`, or past `deadline` at
/// `now`) — the same stale-file cleanup `hook::detect`'s marker pruning
/// already does for the per-session marker files, extended to this
/// directory (hooked in from there) so a supervisor that never claims a
/// request (crashed, or was never actually alive despite a live-looking pid
/// check) does not leave it behind forever. Best-effort; a missing
/// directory is not an error, and a file this process cannot currently read
/// is left for the next sweep rather than guessed at.
pub fn prune_stale_requests(dir: &Path, now: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let stale = match serde_json::from_slice::<Request>(&bytes) {
            Ok(req) => !is_live(&req, now),
            Err(_) => true,
        };
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(deadline: i64) -> Request {
        Request {
            v: REQUEST_V,
            mode: "on".to_owned(),
            sid: "sid-1".to_owned(),
            written_at: 1_000,
            deadline,
            recache_tokens: 150_000,
            remaining_secs: 200,
            vim_mode: Some("insert".to_owned()),
        }
    }

    #[test]
    fn write_then_take_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 4242, &sample(2_000)).unwrap();
        let got = take_request(dir, 4242, 1_500).expect("must read back what was written");
        assert_eq!(got, sample(2_000));
    }

    #[test]
    fn take_request_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 4242, &sample(2_000)).unwrap();
        assert!(take_request(dir, 4242, 1_500).is_some());
        assert!(!path_for(dir, 4242).exists());
        assert!(take_request(dir, 4242, 1_500).is_none());
    }

    #[test]
    fn take_request_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(take_request(tmp.path(), 4242, 1_500).is_none());
    }

    #[test]
    fn take_request_drops_and_removes_a_malformed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(path_for(dir, 4242), b"not json").unwrap();
        assert!(take_request(dir, 4242, 1_500).is_none());
        assert!(
            !path_for(dir, 4242).exists(),
            "malformed file must still be removed"
        );
    }

    #[test]
    fn take_request_drops_an_unknown_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut req = sample(2_000);
        req.v = 99;
        write_request(dir, 4242, &req).unwrap();
        assert!(take_request(dir, 4242, 1_500).is_none());
    }

    #[test]
    fn take_request_drops_an_expired_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 4242, &sample(1_000)).unwrap();
        assert!(
            take_request(dir, 4242, 1_001).is_none(),
            "deadline already passed"
        );
    }

    #[test]
    fn take_request_keeps_a_deadline_exactly_at_now() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 4242, &sample(1_000)).unwrap();
        assert!(
            take_request(dir, 4242, 1_000).is_some(),
            "deadline == now must still be valid"
        );
    }

    #[test]
    fn clear_own_request_removes_only_that_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 111, &sample(2_000)).unwrap();
        write_request(dir, 222, &sample(2_000)).unwrap();
        clear_own_request(dir, 111);
        assert!(!path_for(dir, 111).exists());
        assert!(path_for(dir, 222).exists());
    }

    #[test]
    fn clear_own_request_is_a_noop_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        clear_own_request(tmp.path(), 4242); // must not panic
    }

    #[test]
    fn prune_stale_requests_removes_expired_and_malformed_but_keeps_live() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_request(dir, 1, &sample(500)).unwrap(); // expired at now=1_000
        write_request(dir, 2, &sample(2_000)).unwrap(); // still live
        std::fs::write(path_for(dir, 3), b"not json").unwrap();
        std::fs::write(dir.join("not-a-request.txt"), b"ignore me").unwrap();

        prune_stale_requests(dir, 1_000);

        assert!(!path_for(dir, 1).exists(), "expired request must be pruned");
        assert!(path_for(dir, 2).exists(), "live request must survive");
        assert!(
            !path_for(dir, 3).exists(),
            "malformed request must be pruned"
        );
        assert!(
            dir.join("not-a-request.txt").exists(),
            "non-.json file must be left alone"
        );
    }

    #[test]
    fn prune_stale_requests_missing_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        prune_stale_requests(&tmp.path().join("nope"), 1_000); // must not panic
    }
}
