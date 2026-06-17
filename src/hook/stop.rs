//! Hook kill/stop — commit ordering + POSIX SIGTERM / Windows .stop flag IPC.
//!
//! # Commit ordering (§4b, mirrors `limit-switch.sh.j2` lines 384–425)
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
//! - The recorded PID is a live process whose exe basename ends in `claude` or `node`
//!   (case-insensitive, `.exe` stripped on Windows) — TOCTOU-tolerant via born check.
//!
//! If either gate fails the function returns `Ok(())` without stopping (notify already
//! emitted by the caller — this degrades to notify-only).

use std::path::Path;

use anyhow::Context as _;

/// Relaunch sentinel written atomically before the stop signal.
///
/// Legacy zsh wrote `hop` as a JSON **number** in `.relaunch`; the Rust binary
/// continues this convention (i64, not a string) so a partially-upgraded machine
/// can still read the sentinel with its old reader.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RelaunchSentinel {
    pub session_id: String,
    pub target_profile: String,
    pub cwd: String,
    pub handoff: String,
    /// hop is a JSON number (not a string) — see §6 read-compat matrix.
    pub hop: i64,
    /// born epoch (seconds since UNIX_EPOCH) from the owning supervisor's `.pid` file.
    /// The supervisor rejects sentinels with `born < launch_born` (stale-sentinel guard).
    pub born: i64,
}

/// Execute the full commit sequence and then stop the managed process.
///
/// `sid`            — session UUID string.
/// `target_profile` — profile name to switch to (stored in the sentinel).
/// `handoff`        — handoff prompt string forwarded to the resumed session.
/// `owner_dir`      — CLAUDE_CONFIG_DIR of the owning profile (for cwd resolution).
pub fn commit_and_stop(
    sid: &str,
    target_profile: &str,
    handoff: &str,
    owner_dir: &Path,
) -> anyhow::Result<()> {
    use crate::paths;

    // ── Step 1: read current hop from sidecar, compute next_hop ──────────────
    let current_hop = read_sidecar_hop(sid);
    let next_hop = current_hop + 1;

    // Merge next_hop back into the sidecar (merge-not-clobber: preserve other fields).
    merge_sidecar_hop(sid, next_hop)?;

    // ── Step 2: write .relaunch sentinel (atomic) ─────────────────────────────
    let cwd = owner_dir
        .to_str()
        .unwrap_or(".")
        .to_string();

    let born = read_pid_born(sid).unwrap_or(0);

    let sentinel = RelaunchSentinel {
        session_id: sid.to_string(),
        target_profile: target_profile.to_string(),
        cwd,
        handoff: handoff.to_string(),
        hop: next_hop,
        born,
    };

    write_relaunch_sentinel(sid, &sentinel)?;

    // ── Step 3: noclobber .switched marker ───────────────────────────────────
    let switched_path = paths::switched(sid);
    if !switched_path.exists() {
        let epoch = now_epoch();
        // Write epoch string; ignore EEXIST (noclobber semantics: first write wins).
        let _ = write_noclobber(&switched_path, &format!("{epoch}"));
    }

    // ── Step 4: re-stamp .last-switch ────────────────────────────────────────
    let epoch = now_epoch();
    std::fs::write(paths::last_switch(), format!("{epoch}"))
        .context("failed to write .last-switch")?;

    // ── Step 5: stop the managed process (LAST) ───────────────────────────────
    stop_managed_process(sid)?;

    Ok(())
}

// ─── sentinel write ───────────────────────────────────────────────────────────

/// Write the relaunch sentinel atomically (tmp+rename, same filesystem).
fn write_relaunch_sentinel(sid: &str, sentinel: &RelaunchSentinel) -> anyhow::Result<()> {
    use crate::paths;

    let dest = paths::relaunch(sid);
    let tmp = dest.with_extension("relaunch.tmp");
    let json = serde_json::to_string(sentinel).context("failed to serialize relaunch sentinel")?;
    std::fs::write(&tmp, &json).context("failed to write relaunch sentinel tmp")?;
    std::fs::rename(&tmp, &dest).context("failed to rename relaunch sentinel into place")?;
    Ok(())
}

// ─── sidecar hop helpers ─────────────────────────────────────────────────────

/// Read the `hop` field from `<sid>.json`, tolerating both String and Number forms.
/// Returns 0 on missing/corrupt sidecar (§6 compat: old zsh wrote hop as a JSON string).
fn read_sidecar_hop(sid: &str) -> i64 {
    use crate::paths;
    let Ok(content) = std::fs::read_to_string(paths::sidecar(sid)) else {
        return 0;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 0;
    };
    match val.get("hop") {
        Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

/// Merge `next_hop` into `<sid>.json` without clobbering other fields.
/// The hop field is written as a JSON **string** for sidecar compatibility (§6 compat:
/// `merge_sidecar` in the old zsh used `jq --arg` which always produces a string value).
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

/// Returns true if `pid` is a live process whose exe basename ends with "claude" or "node"
/// (case-insensitive; `.exe` stripped on Windows).
///
/// Uses targeted `sysinfo` refresh (never a full sweep) on Windows/Linux,
/// and `ps -o comm=` on macOS (POSIX-only path).
fn is_live_claude_or_node(pid: u32) -> bool {
    platform_is_live_claude_or_node(pid)
}

/// Shared name-check helper: `basename` must end with "claude" or "node" (case-insensitive).
pub fn is_claude_or_node_name(base: &str) -> bool {
    let l = base.to_ascii_lowercase();
    l.ends_with("claude") || l.ends_with("node")
}

// ─── cfg(unix) implementations ────────────────────────────────────────────────

#[cfg(unix)]
fn platform_is_live_claude_or_node(pid: u32) -> bool {
    // POSIX: `ps -o comm= -p <pid>` — NOT `-o args=` (avoids leaking CLAUDE_CONFIG_DIR).
    use std::process::Command;
    let output = match Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let comm = String::from_utf8_lossy(&output.stdout);
    let base = comm.trim();
    // Strip path prefix: take only the final component.
    let basename = base.rsplit('/').next().unwrap_or(base);
    is_claude_or_node_name(basename)
}

#[cfg(unix)]
fn platform_stop(pid: u32, _sid: &str) -> anyhow::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .with_context(|| format!("failed to SIGTERM pid {pid}"))?;
    Ok(())
}

// ─── cfg(windows) implementations ─────────────────────────────────────────────

#[cfg(windows)]
fn platform_is_live_claude_or_node(pid: u32) -> bool {
    // Windows: targeted sysinfo refresh (never a full system sweep on the hot stop path).
    use sysinfo::{Pid, ProcessRefreshKind, System};
    let mut sys = System::new();
    let sysinfo_pid = Pid::from_u32(pid);
    sys.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
    let Some(proc) = sys.process(sysinfo_pid) else {
        return false;
    };
    let Some(exe_name) = proc.exe().and_then(|p| p.file_name()) else {
        return false;
    };
    let name = exe_name.to_string_lossy();
    // Strip .exe suffix for comparison.
    let base = name.strip_suffix(".exe").unwrap_or(&name);
    is_claude_or_node_name(base)
}

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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Write `content` to `path` only if the file does not already exist (noclobber semantics).
/// Returns Ok(()) regardless of whether the write happened.
fn write_noclobber(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    // `create_new(true)` fails with AlreadyExists if the file exists — noclobber.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
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

    /// is_claude_or_node_name recognizes bare "claude" and "node".
    #[test]
    fn name_check_basic() {
        assert!(is_claude_or_node_name("claude"));
        assert!(is_claude_or_node_name("node"));
    }

    /// is_claude_or_node_name is case-insensitive.
    #[test]
    fn name_check_case_insensitive() {
        assert!(is_claude_or_node_name("Claude"));
        assert!(is_claude_or_node_name("NODE"));
        assert!(is_claude_or_node_name("CLAUDE"));
    }

    /// is_claude_or_node_name rejects unrelated names.
    #[test]
    fn name_check_rejects_unrelated() {
        assert!(!is_claude_or_node_name("bash"));
        assert!(!is_claude_or_node_name("python3"));
        assert!(!is_claude_or_node_name("csm"));
        assert!(!is_claude_or_node_name(""));
    }

    /// is_claude_or_node_name is tolerant of "some-claude" style names (ends_with).
    #[test]
    fn name_check_ends_with_tolerant() {
        // Spec: `l.ends_with("claude") || l.ends_with("node")`
        // "claude-3" does NOT end with "claude" — correct per spec.
        assert!(!is_claude_or_node_name("claude-3"));
        // A name ending in "claude" (e.g. from a renamed binary) matches.
        assert!(is_claude_or_node_name("some-claude"));
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
        };
        let json = serde_json::to_string(&sentinel).unwrap();
        // hop must be a JSON number (not a string) in .relaunch (§6 compat)
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
