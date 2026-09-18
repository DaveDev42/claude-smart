//! Hook kill/stop — commit ordering + POSIX SIGTERM / Windows .stop flag IPC.
//!
//! # Commit ordering (matches the legacy shell implementation)
//!
//! 1. merge-sidecar `hop` (increment next_hop into `<sid>.json`)
//! 2. write `.relaunch` sentinel (atomic tmp+rename)
//! 3. noclobber-create `.switched` marker
//! 4. re-stamp `.last-switch`
//! 5. stop signal — LAST so the supervisor always finds a complete sentinel:
//!    - POSIX (`cfg(unix)`): `kill(pid, SIGTERM)` via nix
//!    - Windows (`cfg(windows)`): write `<sid>.stop` presence flag; supervisor polls
//!
//! # Managed-session gate
//!
//! We stop **only** a session this loop manages. Both conditions must hold:
//! - `<sid>.pid` exists and is parseable as `<pid> <born>`.
//! - The recorded PID is a live process whose name, exe basename, or argv[0]
//!   basename ends in `claude` or `node` (case-insensitive, `.exe` stripped) —
//!   TOCTOU-tolerant via born check.
//!
//! If either gate fails the function returns `Ok(())` without stopping (notify already
//! emitted by the caller — this degrades to notify-only).

use std::path::Path;

use anyhow::Context as _;

/// The relaunch sentinel format is owned by `platform::relaunch` (the supervisor's
/// reader lives there too). Re-export it so the hook writes the EXACT same wire
/// format the supervisor reads — a single SSOT prevents silent field/format drift.
pub use crate::platform::relaunch::RelaunchSentinel;

/// Execute the full commit sequence and then stop the managed process.
///
/// `sid`            — session UUID string.
/// `target_profile` — profile name to switch to (stored in the sentinel).
/// `handoff`        — handoff prompt string forwarded to the resumed session.
/// `cwd`            — working directory from the hook input (not owner_dir).
/// `born`           — born epoch read from the PID file by classify().
/// `model_override` — `Some(model)` for a same-account model fallback (see
///                     `crate::hook::detect::fable_fallback_model`);
///                     `None` for an ordinary account switch.
///
/// The commit ordering (matches the legacy shell implementation) for an
/// ordinary account switch (`model_override: None`):
///   1. merge-sidecar hop
///   2. write .relaunch sentinel (atomic tmp+rename)
///   3. noclobber-create .switched marker
///   4. re-stamp .last-switch
///   5. stop signal LAST (POSIX SIGTERM / Windows .stop flag)
///
/// `model_override: Some(_)` skips steps 1, 3, and 4 — the account-switch hop
/// bump, `.switched` marker, and machine-wide cooldown restamp all belong to
/// the account-switch path and must stay untouched by a relaunch that never
/// switched accounts (see `fable_fallback_model`'s loop-safety doc). In their
/// place it (re)writes `<sid>.model-fallback` — exclusively claimed first on
/// the statusline entry point, see `claim_model_fallback` — and writes the
/// sentinel with `hop` equal to the *current* sidecar hop (unchanged, not
/// bumped).
/// Step 2 (sentinel) and step 5 (stop signal) run exactly as before either
/// way.
pub fn commit_and_stop(
    sid: &str,
    target_profile: &str,
    handoff: &str,
    cwd: &str,
    born: i64,
    model_override: Option<&str>,
) -> anyhow::Result<()> {
    use crate::paths;

    // ── Step 1: read current hop from sidecar, compute next_hop ──────────────
    // A model fallback never bumps the hop or touches the sidecar — the
    // sentinel's hop stays exactly what it already was.
    let current_hop = crate::hook::read_sidecar_hop(sid);
    let hop = if model_override.is_some() {
        current_hop
    } else {
        let next_hop = current_hop + 1;
        // Merge next_hop back into the sidecar (merge-not-clobber: preserve other fields).
        // Shell: `"$HELPER" merge-sidecar "$session_id" hop "$next_hop"`
        merge_sidecar_hop(sid, next_hop)?;
        next_hop
    };

    // ── Step 2: write .relaunch sentinel (atomic) ─────────────────────────────
    // Shell: `"$HELPER" write-relaunch ...`
    // born is passed from classify() (already read from the pidfile there).
    let actual_born = if born != 0 {
        born
    } else {
        read_pid_born(sid).unwrap_or(0)
    };

    let sentinel = RelaunchSentinel {
        session_id: sid.to_string(),
        target_profile: target_profile.to_string(),
        cwd: cwd.to_string(),
        handoff: handoff.to_string(),
        hop,
        born: actual_born,
        model_override: model_override.map(str::to_string),
    };

    crate::platform::relaunch::write_relaunch(&paths::relaunch(sid), &sentinel)?;

    if model_override.is_some() {
        // Marker: this session fell back to the fallback model on a Fable
        // cap, current as of now. The statusline entry point
        // ([`crate::hook::run_from_statusline`]) already exclusively claimed
        // this marker via `claim_model_fallback` before ever calling here —
        // by the time `classify_with` returned this decision it had already
        // established no marker for the CURRENT week_fable window survives
        // (a stale or corrupt one is removed at that point, see
        // `crate::hook::detect::model_fallback_marker_is_stale`), so this
        // call is always writing into a slot that is either freshly claimed
        // or empty. The direct hook entry point never claims first, so this
        // write is what actually creates the marker there. Either way this
        // just (re)writes the current epoch, atomically (tmp + rename) so a
        // reader never observes a partially written file. Deliberately NOT
        // `.switched`/`.last-switch` — those belong to the account-switch
        // hop budget and cooldown, which a same-account model change must
        // never consume.
        let _ = write_atomic(&paths::model_fallback(sid), &now_epoch().to_string());
    } else {
        // ── Step 3: noclobber .switched marker ───────────────────────────────
        let switched_path = paths::switched(sid);
        if !switched_path.exists() {
            let epoch = now_epoch();
            // Write epoch string; ignore EEXIST (noclobber semantics: first write wins).
            let _ = write_noclobber(&switched_path, &format!("{epoch}"));
        }

        // ── Step 4: re-stamp .last-switch ────────────────────────────────────
        let epoch = now_epoch();
        std::fs::write(paths::last_switch(), format!("{epoch}"))
            .context("failed to write .last-switch")?;
    }

    // ── Step 5: stop the managed process (LAST) ───────────────────────────────
    stop_managed_process(sid)?;

    Ok(())
}

// ─── sidecar hop helpers ─────────────────────────────────────────────────────

/// Merge `next_hop` into `<sid>.json` without clobbering other fields.
/// The hop field is written as a JSON **string** for sidecar compatibility:
/// `merge_sidecar` in the legacy zsh implementation used `jq --arg`, which
/// always produces a string value.
fn merge_sidecar_hop(sid: &str, next_hop: i64) -> anyhow::Result<()> {
    use crate::paths;

    let path = paths::sidecar(sid);

    // Read existing sidecar or start from empty object.
    let mut val: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };

    // Ensure val is an object; reset to {} on corrupt non-object.
    if !val.is_object() {
        val = serde_json::json!({});
    }

    // Write hop as a string (jq --arg compat).
    val["hop"] = serde_json::Value::String(next_hop.to_string());

    // Atomic tmp+rename.
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string(&val).context("failed to serialize sidecar")?;
    std::fs::write(&tmp, &json).context("failed to write sidecar tmp")?;
    std::fs::rename(&tmp, &path).context("failed to rename sidecar into place")?;

    Ok(())
}

// ─── pid helpers ─────────────────────────────────────────────────────────────

/// Read `<born>` from `<sid>.pid` (`<pid> <born>`). Returns None on missing/parse failure.
fn read_pid_born(sid: &str) -> Option<i64> {
    use crate::paths;
    let content = std::fs::read_to_string(paths::pid_file(sid)).ok()?;
    let mut parts = content.split_whitespace();
    let _pid: u32 = parts.next()?.parse().ok()?;
    let born: i64 = parts.next()?.parse().ok()?;
    Some(born)
}

/// Read `<pid>` from `<sid>.pid`. Returns None on missing/parse failure.
fn read_pid(sid: &str) -> Option<u32> {
    use crate::paths;
    let content = std::fs::read_to_string(paths::pid_file(sid)).ok()?;
    let pid: u32 = content.split_whitespace().next()?.parse().ok()?;
    Some(pid)
}

// ─── process stop ─────────────────────────────────────────────────────────────

/// Stop the managed process identified by `<sid>.pid`.
///
/// Managed-session gate: only stops if the PID in the file is a live claude/node process.
fn stop_managed_process(sid: &str) -> anyhow::Result<()> {
    let Some(pid) = read_pid(sid) else {
        // No pidfile — session unmanaged; skip (already notified).
        return Ok(());
    };

    if !is_live_claude_or_node(pid) {
        // PID is not a live claude/node — do not kill unrelated processes.
        return Ok(());
    }

    // Passed the managed-session gate: perform the platform-appropriate stop.
    platform_stop(pid, sid)
}

// ─── platform stop implementations ───────────────────────────────────────────

/// Public wrapper for detect.rs to call without reimplementing the check.
/// Returns true if `pid` is a live process whose name, exe basename, or argv[0]
/// basename ends with "claude" or "node" (case-insensitive; `.exe` stripped).
///
/// Uses a targeted `sysinfo` refresh (never a full sweep) on every platform.
pub fn check_is_live_claude_or_node(pid: u32) -> bool {
    is_live_claude_or_node(pid)
}

fn is_live_claude_or_node(pid: u32) -> bool {
    // Cross-platform: `SysinfoProcCheck` does a single-process refresh + a
    // name/exe/argv[0] match (no `ps` spawn). Wired identically on every OS.
    use crate::platform::proc_check::ProcCheck;
    crate::platform::proc_check::SysinfoProcCheck::is_live_claude_or_node(pid)
}

// ─── cfg(unix) implementations ────────────────────────────────────────────────

#[cfg(unix)]
fn platform_stop(pid: u32, _sid: &str) -> anyhow::Result<()> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .with_context(|| format!("failed to SIGTERM pid {pid}"))?;
    Ok(())
}

// ─── cfg(windows) implementations ─────────────────────────────────────────────

#[cfg(windows)]
fn platform_stop(pid: u32, sid: &str) -> anyhow::Result<()> {
    use crate::paths;
    // Windows IPC: write the <sid>.stop presence flag.
    // The supervisor polls for this file while claude.exe runs; on detection it
    // performs: delete flag → CTRL_BREAK_EVENT → grace → TerminateProcess fallback.
    let stop_path = paths::stop_flag(sid);
    std::fs::write(&stop_path, b"")
        .with_context(|| format!("failed to write stop flag for pid {pid}"))?;
    Ok(())
}

// ─── utility helpers ──────────────────────────────────────────────────────────

fn now_epoch() -> i64 {
    crate::epoch::now_secs() as i64
}

/// Claim the `.switched` marker for `sid` *before* committing, for callers
/// whose invocations overlap (`hook::run_from_statusline` — the statusline
/// ticks about once a second and each runs as its own backgrounded process,
/// so two of them can both reach `LimitSwitch` for the same session).
/// `true` means this caller owns the switch; `false` means another one got
/// there first and this caller must do nothing. [`commit_and_stop`]'s own
/// step 3 then finds the marker present and leaves it alone. The hook path
/// keeps its commit-then-mark order: one Stop event = one hook process.
pub(crate) fn claim_switched(sid: &str) -> bool {
    claim_marker(&crate::paths::switched(sid))
}

/// Claim the `.model-fallback` marker for `sid` *before* committing — for a
/// same-account model fallback (see
/// [`crate::hook::detect::fable_fallback_model`]). Deliberately a SEPARATE
/// marker/claim from `.switched`: a model fallback must never touch
/// `.switched`, or it would burn this session's one-shot account-switch
/// budget on a relaunch that never switched accounts.
///
/// Same exclusivity as [`claim_switched`] — a plain [`claim_marker`] claim:
/// `true` means this caller owns the fallback and must commit; `false` means
/// another overlapping tick already claimed it and this caller must do
/// nothing. Two overlapping ticks for the same session must never both
/// commit (each would write its own relaunch sentinel and send its own stop
/// signal to the same supervised process). A leftover marker from an EARLIER
/// `week_fable` window is not this function's concern: `classify_with`
/// removes a stale or unparseable marker itself, before ever reaching a
/// `LimitSwitch` decision with `model_override: Some(_)` (see
/// [`crate::hook::detect::model_fallback_marker_is_stale`]), so by the time a
/// caller reaches here the marker, if any, is either fresh (another tick's
/// legitimate claim, which this call must lose to) or absent.
pub(crate) fn claim_model_fallback(sid: &str) -> bool {
    claim_marker(&crate::paths::model_fallback(sid))
}

/// Exclusively claim `path` with the current epoch as content. `true` iff
/// this call created it; `false` on any failure, including "already
/// exists" (another claimant got there first) and an unwritable parent dir.
///
/// Writes the content to a private sibling tmp file first, then atomically
/// links it into place (`hard_link` fails with `AlreadyExists` exactly like
/// `create_new` would, so exclusivity is unchanged) rather than
/// `create_new` + `write_all` directly. That ordering means a claimant can
/// never observe a truncated/partial marker if the process dies between
/// opening the file and finishing the write — a failure mode a plain
/// `create_new` + `write_all` on `.model-fallback` would have left
/// reachable (see [`crate::hook::detect::model_fallback_marker_epoch`]'s
/// doc).
fn claim_marker(path: &Path) -> bool {
    let tmp = tmp_sibling(path);
    if std::fs::write(&tmp, now_epoch().to_string()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    let claimed = std::fs::hard_link(&tmp, path).is_ok();
    let _ = std::fs::remove_file(&tmp);
    claimed
}

/// A private sibling path next to `path`, namespaced by this process's PID
/// so concurrent claimants (separate processes — see [`claim_marker`]'s doc)
/// never write each other's tmp file. Shared by [`claim_marker`] and
/// [`write_atomic`].
fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("marker");
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

/// Write `content` to `path` atomically via tmp + rename (the same pattern
/// [`crate::platform::relaunch::write_relaunch`] uses), overwriting whatever
/// was there. Unlike [`claim_marker`] this makes no exclusivity claim — it
/// is for a caller that already owns the slot (or knows no one else can be
/// writing it) and just wants to refresh its content without a reader ever
/// observing a partial write.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = tmp_sibling(path);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Write `content` to `path` only if the file does not already exist (noclobber semantics).
/// Returns Ok(()) regardless of whether the write happened.
fn write_noclobber(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    // `create_new(true)` fails with AlreadyExists if the file exists — noclobber.
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(content.as_bytes())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // File exists — noclobber: first write wins, silently skip.
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // The exe-basename name check (claude/node, case-insensitive, ends_with)
    // lives in `platform::proc_check` and is tested there; this module delegates
    // to `SysinfoProcCheck` rather than re-implementing it.

    /// claim_marker: exactly one of two overlapping claimants wins, and the
    /// loser sees `false` rather than an error — the statusline tick's
    /// "only one tick commits" rule.
    #[test]
    fn claim_marker_first_caller_wins() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sid.switched");
        assert!(claim_marker(&path));
        assert!(!claim_marker(&path));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.parse::<i64>().is_ok(),
            "marker holds an epoch: {content:?}"
        );
    }

    #[test]
    fn claim_marker_unwritable_dir_is_false() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no-such-subdir").join("sid.switched");
        assert!(!claim_marker(&path));
    }

    /// `claim_model_fallback` must have the same exclusivity `claim_switched`
    /// has: two overlapping claims for the same session, exactly one wins
    /// and the loser must not overwrite. Drives
    /// the real public entry point (keyed by `sid` via
    /// `paths::model_fallback`, which reads `HOME`), not just the shared
    /// `claim_marker` primitive.
    #[test]
    fn claim_model_fallback_first_caller_wins() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".claude.shared").join("smart")).unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let sid = "sid-claim-race-0001";
            assert!(claim_model_fallback(sid), "first claimant must win");
            assert!(
                !claim_model_fallback(sid),
                "an overlapping second claimant must lose, not overwrite"
            );
            let content = std::fs::read_to_string(crate::paths::model_fallback(sid)).unwrap();
            assert!(
                content.parse::<i64>().is_ok(),
                "marker holds a complete, parseable epoch: {content:?}"
            );
        });
    }

    /// write_noclobber: first write succeeds; second write is silently ignored.
    #[test]
    fn noclobber_first_write_wins() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker");
        write_noclobber(&path, "first").unwrap();
        write_noclobber(&path, "second").unwrap(); // must not panic
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "first");
    }

    /// RelaunchSentinel round-trips through serde_json with hop as a JSON number.
    #[test]
    fn relaunch_sentinel_hop_is_number() {
        let sentinel = RelaunchSentinel {
            session_id: "test-sid".to_string(),
            target_profile: "work".to_string(),
            cwd: "/tmp/cwd".to_string(),
            handoff: "resume".to_string(),
            hop: 1,
            born: 1718000000,
            model_override: None,
        };
        let json = serde_json::to_string(&sentinel).unwrap();
        // hop must be a JSON number (not a string) in .relaunch
        assert!(
            json.contains("\"hop\":1"),
            "hop should be a JSON number in .relaunch: {json}"
        );
        let back: RelaunchSentinel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "test-sid");
        assert_eq!(back.hop, 1);
        assert_eq!(back.born, 1718000000);
    }

    /// merge_sidecar_hop writes hop as a JSON string (jq --arg compat for sidecar).
    #[test]
    fn sidecar_hop_serialized_as_string() {
        // Test the serde contract directly: sidecar hop must be a JSON string.
        let mut val = serde_json::json!({
            "sessionId": "abc",
            "permissionMode": "default"
        });
        val["hop"] = serde_json::Value::String(1_i64.to_string());
        let json = serde_json::to_string(&val).unwrap();
        // hop must be a JSON string in the sidecar (old zsh used jq --arg)
        assert!(
            json.contains("\"hop\":\"1\""),
            "hop should be a JSON string in sidecar: {json}"
        );
    }

    /// RelaunchSentinel hop is i64 (not a string), matching .relaunch format.
    #[test]
    fn relaunch_sentinel_born_and_hop_types() {
        let json = r#"{"session_id":"s","target_profile":"p","cwd":"/","handoff":"h","hop":2,"born":1234567890}"#;
        let s: RelaunchSentinel = serde_json::from_str(json).unwrap();
        assert_eq!(s.hop, 2i64);
        assert_eq!(s.born, 1234567890i64);
    }
}
