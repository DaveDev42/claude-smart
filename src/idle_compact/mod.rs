//! idle-compact: send `/compact` to an idle Claude Code session shortly
//! before its prompt cache expires, so the request after the gap re-writes
//! a compacted context instead of the full one. Opt-in — see
//! [`crate::config::IdleCompactMode`] (`csm config set idle-compact
//! off|dry-run|on`, default `off`).
//!
//! ## Entry points
//!
//! No new hook registration and no daemon: both `csm usage capture`
//! ([`crate::cmd::usage`]) and `csm statusline` ([`crate::statusline`])
//! already receive the statusLine JSON payload on stdin on every refresh.
//! Both call [`run_from_raw`] ([`tick`]'s one I/O shell).
//!
//! ## Turn-ended marker
//!
//! `csm hook` calls [`mark_turn_ended`] whenever it handles a turn-boundary
//! event (`Stop`, or the legacy shape with no event name — see
//! `hook::detect::is_turn_boundary`): it stamps `<sid>.idle`
//! ([`paths::idle`]) with the current epoch and clears `<sid>.idle-compacted`
//! ([`paths::idle_compacted`]), so the idle period that just ended can no
//! longer suppress the next one. This is plain file I/O, which Invariant 6
//! allows from every hook event.
//!
//! ## Hand-off, not delivery
//!
//! csm itself types nothing into a Claude Code session any more. `tick`'s
//! pure decision core gates on the same conditions as before (mode,
//! fired-this-idle-period, cache warm, remaining TTL, recache estimate, turn
//! ended, not busy — see [`tick`]'s doc) and then hands off to whatever is
//! running as csm's own pty-relay supervisor: a request file written by
//! [`request::write_request`] when `CSM_SUPERVISOR_PID` names a live
//! process, or nothing (logged `no-delivery-path`) when it does not. Reading
//! no `ORCA_*`/`ZELLIJ_*`/`WEZTERM_*` variable and running no external
//! program are both structural now — the code paths that used to do either
//! are gone, not merely unused.
//!
//! The supervisor side ([`supervisor`], the pty relay's observer) reads a request back with
//! [`request::take_request`], checks a screen model and the session-status
//! veto ([`status`]) and steps the pure delivery state machine in
//! [`deliver`] to decide what to type, if anything. [`log_outcome`] is the
//! one log-line format both this module's own tick and that future
//! supervisor append to `idle-compact.log`.
//!
//! ## Busy check
//!
//! See [`tick`]'s doc for the transcript-tail busy check, including how an
//! interrupted turn (a `[Request interrupted by user` row) counts as a turn
//! end with no `Stop` hook.

pub mod deliver;
mod log;
pub mod request;
pub mod status;
#[cfg(unix)]
pub mod supervisor;
mod tick;

pub use log::{LogFields, log_outcome};
pub use tick::run_from_raw;

use std::path::Path;

use crate::paths;

/// The env var a relay-mode `csm run` sets in claude's environment: its own
/// pid, so `tick` can hand a request to it. The relay launcher (built
/// separately) defines this same name on its side; the two are unified at
/// merge, not duplicated by accident.
pub(crate) const SUPERVISOR_PID_ENV: &str = "CSM_SUPERVISOR_PID";

// ─── turn-ended marker (written by `csm hook`) ───────────────────────────────

/// Called by `csm hook` when it handles a turn-boundary event for `sid`:
/// stamps [`paths::idle`] with the current epoch and clears
/// [`paths::idle_compacted`] (the one-shot marker for the idle period that
/// just ended, so a fresh one can fire in the next). Best-effort — a
/// failure here must never fail the hook.
pub fn mark_turn_ended(sid: &str) {
    let _ = write_marker(&paths::idle(sid), &now_epoch().to_string());
    let _ = std::fs::remove_file(paths::idle_compacted(sid));
}

fn now_epoch() -> i64 {
    crate::epoch::now_secs() as i64
}

/// Write `content` to `path` atomically (tmp + rename), overwriting
/// whatever was there. Mirrors `hook::stop::write_atomic` (module-private
/// there, so duplicated here rather than exposed across modules for one
/// caller). Used only for the small bare-epoch marker files under
/// `smart_dir` ([`paths::idle`], [`paths::idle_compacted`]) — [`request`]
/// writes its JSON request files through `orca::fsx::write_atomic` instead,
/// matching the relay sentinel's own writer.
fn write_marker(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("marker");
    let tmp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_turn_ended_writes_idle_and_clears_compacted_marker() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let sid = "sid-1";
            std::fs::create_dir_all(paths::smart_dir_no_create()).unwrap();
            std::fs::write(paths::idle_compacted(sid), "1700000000").unwrap();
            assert!(paths::idle_compacted(sid).exists());

            mark_turn_ended(sid);

            assert!(paths::idle(sid).exists(), "idle marker must be written");
            let content = std::fs::read_to_string(paths::idle(sid)).unwrap();
            let stamped: i64 = content.trim().parse().expect("idle marker holds an epoch");
            assert!(stamped > 0);
            assert!(
                !paths::idle_compacted(sid).exists(),
                "idle-compacted marker must be cleared on a turn boundary"
            );
        });
    }

    #[test]
    fn mark_turn_ended_is_a_noop_when_no_compacted_marker_exists() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let sid = "sid-2";
            mark_turn_ended(sid); // must not panic/error with nothing to clear
            assert!(paths::idle(sid).exists());
            assert!(!paths::idle_compacted(sid).exists());
        });
    }

    #[test]
    fn mark_turn_ended_overwrites_a_stale_idle_marker() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let sid = "sid-3";
            std::fs::create_dir_all(paths::smart_dir_no_create()).unwrap();
            std::fs::write(paths::idle(sid), "1").unwrap();
            mark_turn_ended(sid);
            let content = std::fs::read_to_string(paths::idle(sid)).unwrap();
            let stamped: i64 = content.trim().parse().unwrap();
            assert!(stamped > 1, "a later Stop must refresh the stamp");
        });
    }
}
