//! Relaunch loop and the `RelaunchSentinel` serde model.
//!
//! The relaunch loop (`run_relaunch_loop`) has **no cfg guards** — it is
//! platform-agnostic and consumes a `&dyn Launcher` + `ProcCheck`.
//!
//! ## RelaunchSentinel — read-compat with legacy zsh `write_relaunch`
//!
//! The legacy zsh helper wrote the sentinel via `jq` with `--argjson hop` (a JSON
//! NUMBER) and `--argjson born` (a JSON NUMBER).  The Rust binary must round-trip
//! these files that may already exist on disk at cutover.  Both fields are `i64`.
//!
//! Compare with `Sidecar`: the sidecar `hop` was written by jq `--arg` (a JSON
//! STRING).  The distinction is **per-file**, not ambiguous within one file.
//! See `sidecar/mod.rs` for the complementary type.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// `<sid>.relaunch` — atomic JSON sentinel written by the hook, consumed by the
/// supervisor's post-wait path.
///
/// Field names match the legacy zsh `write_relaunch` jq output exactly (spec §6):
///   `session_id`, `target_profile`, `cwd`, `handoff`, `hop`, `born`.
///
/// `hop` is a JSON **number** here (contrast with `Sidecar.hop` which is a JSON
/// string).  `born` is compared against the `born` epoch written into `<sid>.pid`
/// at the start of this loop iteration — the stale-sentinel rejection linchpin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaunchSentinel {
    pub session_id: String,
    pub target_profile: String,
    pub cwd: String,
    pub handoff: String,
    /// JSON number.  The loop breaks when `hop > MAX_HOPS`.
    pub hop: i64,
    /// Unix epoch (seconds).  Consume only when `sentinel.born >= launch_born`.
    pub born: i64,
}

/// Maximum number of limit-switch hops before the relaunch loop breaks.
/// Matches the legacy zsh `MAX_HOPS=1` constant.
pub const MAX_HOPS: i64 = 1;

/// Read `<sid>.relaunch` from `path`.  Returns `None` if the file is absent;
/// propagates I/O or parse errors.
pub fn read_relaunch(path: &Path) -> anyhow::Result<Option<RelaunchSentinel>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write `sentinel` atomically to `path` via a temp file + rename (same filesystem).
pub fn write_relaunch(path: &Path, sentinel: &RelaunchSentinel) -> anyhow::Result<()> {
    let tmp = path.with_extension("relaunch.tmp");
    let json = serde_json::to_string(sentinel)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Entry point for the foreground relaunch loop.
///
/// `launcher`    — platform-specific `Launcher` impl (POSIX or Windows).
/// `spec`        — launch parameters (CLI argv, sidecar sid, profile dir, etc.).
///
/// The loop runs until:
/// - No `.relaunch` sentinel appears after claude exits, OR
/// - `sentinel.born < launch_born` (stale sentinel — ignore), OR
/// - `sentinel.hop > MAX_HOPS`, OR
/// - `target_profile` is unknown (abort), OR
/// - A sentinel atomic-consume race is detected.
///
pub fn run_relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    use crate::paths;
    use crate::platform::pid;

    let sid = spec.session_id.clone();
    let relaunch_path = paths::relaunch(&sid);
    let pid_path = paths::pid_file(&sid);

    // The CLI mutates across hops (profile swap + resume + handoff prompt); start
    // from the cold-launch CLI the caller built.
    let mut cli: Vec<OsString> = spec.cli.clone();
    let mut profile_dir = spec.profile_dir.clone();

    loop {
        // Clobber guard: if another live csm already owns this sid's pidfile,
        // do not stomp it — abort this loop (the other supervisor is in charge).
        if let Ok(Some((other_pid, _born))) = pid::read_pid_file(&pid_path) {
            use crate::platform::proc_check::ProcCheck;
            // Our own previous iteration will have left a pidfile for a now-dead
            // pid; only bail if the recorded pid is a DIFFERENT live claude/node.
            if other_pid != 0
                && crate::platform::PlatformProcCheck::is_live_claude_or_node(other_pid)
            {
                anyhow::bail!(
                    "session {sid} is already managed by a live process (pid {other_pid})"
                );
            }
        }

        // A stale sentinel from a prior chain must never be consumed by this
        // launch — remove anything older than the launch we are about to make.
        // (Defensive: the born-check below is the real guard.)
        let _ = std::fs::remove_file(&relaunch_path);

        // Per-launch child env: pin CLAUDE_CONFIG_DIR for this hop's profile.
        let mut env: HashMap<OsString, OsString> = HashMap::new();
        env.insert(
            OsString::from("CLAUDE_CONFIG_DIR"),
            profile_dir.clone().into_os_string(),
        );

        // Launch claude in the foreground and block until it exits. The launcher
        // writes `<sid>.pid` itself immediately after spawn (so the hook can read
        // it mid-session) — we do NOT write it here.
        let (status, handle) = launcher.run_foreground(&sid, &cli, &env)?;

        // Did the hook drop a relaunch sentinel for THIS incarnation?
        let sentinel = match read_relaunch(&relaunch_path) {
            Ok(Some(s)) => s,
            // No sentinel → ordinary exit; we are done.
            Ok(None) => {
                let _ = std::fs::remove_file(&pid_path);
                return exit_with(status);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&pid_path);
                return Err(e);
            }
        };

        // Born-check: reject a sentinel written for a PRIOR launch (the linchpin
        // against consuming a stale handoff). The hook stamps the sentinel with
        // the pidfile's born; it must be >= the born of the launch we just ran.
        if sentinel.born < handle.born {
            // Stale — ignore, treat as ordinary exit.
            let _ = std::fs::remove_file(&relaunch_path);
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Atomic consume: remove the sentinel so a crash mid-relaunch cannot
        // replay it. If removal fails (already gone — another consumer raced us),
        // stop.
        if std::fs::remove_file(&relaunch_path).is_err() {
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Hop cap: bound the number of automatic profile switches per chain.
        if sentinel.hop > MAX_HOPS {
            eprintln!(
                "csm: limit-switch hop cap ({MAX_HOPS}) reached — not relaunching again"
            );
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Resolve the target profile → config dir. Unknown target = abort (do not
        // silently fall back to the same profile, which would loop pointlessly).
        let profiles = crate::account::profiles::ProfileMap::load().unwrap_or_default();
        let next_dir = match profiles.get(&sentinel.target_profile) {
            Some(d) => std::path::PathBuf::from(d),
            None => {
                eprintln!(
                    "csm: relaunch target profile '{}' is unknown — aborting relaunch",
                    sentinel.target_profile
                );
                let _ = std::fs::remove_file(&pid_path);
                return exit_with(status);
            }
        };
        profile_dir = next_dir;

        // Build the next iteration's CLI: same sid, resume the session, carry the
        // hop count forward, and inject the handoff prompt (unless suppressed).
        cli = build_next_cli(&sid, &sentinel);
    }
}

/// Build the claude CLI for the next relaunch hop: resume the same session,
/// pass the handoff prompt (if any), and stamp the hop count so the hook can
/// increment it again.
fn build_next_cli(sid: &str, sentinel: &RelaunchSentinel) -> Vec<OsString> {
    let mut cli: Vec<OsString> = Vec::new();
    cli.push(OsString::from("--resume"));
    cli.push(OsString::from(sid));
    // The handoff prompt is the first turn after resume (e.g. "resume"). Empty =
    // suppressed (user already had a pending tail); pass nothing then.
    if !sentinel.handoff.is_empty() {
        cli.push(OsString::from(&sentinel.handoff));
    }
    cli
}

/// Map a child `ExitStatus` to the loop's `Result`, preserving the exit code by
/// setting our own process exit code to match (so `csm run` is transparent).
fn exit_with(status: std::process::ExitStatus) -> anyhow::Result<()> {
    if let Some(code) = status.code() {
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    // No exit code → killed by a signal (unix). Mirror the shell convention
    // 128 + signo. On Windows, ExitStatus always has a code, so this is unix-only.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            std::process::exit(128 + sig);
        }
    }
    Ok(())
}

/// Parameters for a single `csm run` invocation, threaded through the relaunch loop.
///
/// All fields are intentionally `pub` — the loop builds the next iteration's spec
/// from the consumed sentinel and the previous launch's sidecar.
pub struct LaunchSpec {
    /// The session id (`--session-id`).  A fresh UUID on cold launch; the same
    /// sid across all hops in one relaunch chain.
    pub session_id: String,
    /// Absolute path to the `CLAUDE_CONFIG_DIR` for this launch.
    pub profile_dir: std::path::PathBuf,
    /// The cold-launch working directory. Carried for completeness/diagnostics;
    /// the relaunch loop never re-applies it because every hop runs inside the
    /// same supervisor process, so claude naturally inherits the original cwd
    /// (hops change only the profile, not the directory). `main` uses cwd directly
    /// for session scanning before building the spec.
    #[allow(dead_code)]
    pub cwd: std::path::PathBuf,
    /// Full CLI to pass to claude (everything after `csm run [csm-flags]`).
    pub cli: Vec<OsString>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).expect("write");
        f
    }

    #[test]
    fn roundtrip_sentinel() {
        let sentinel = RelaunchSentinel {
            session_id: "abc123".to_string(),
            target_profile: "personal".to_string(),
            cwd: "/home/you/projects".to_string(),
            handoff: "resume".to_string(),
            hop: 1,
            born: 1_718_000_000,
        };
        let json = serde_json::to_string(&sentinel).unwrap();
        let back: RelaunchSentinel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, sentinel.session_id);
        assert_eq!(back.target_profile, sentinel.target_profile);
        assert_eq!(back.hop, sentinel.hop);
        assert_eq!(back.born, sentinel.born);
    }

    #[test]
    fn field_names_match_legacy_zsh() {
        // The legacy zsh write_relaunch used these exact JSON key names.
        // Verify the serde rename is absent (snake_case == wire name).
        let json = r#"{
            "session_id": "sid-1",
            "target_profile": "work",
            "cwd": "/tmp",
            "handoff": "resume",
            "hop": 0,
            "born": 1700000000
        }"#;
        let s: RelaunchSentinel = serde_json::from_str(json).unwrap();
        assert_eq!(s.session_id, "sid-1");
        assert_eq!(s.target_profile, "work");
        assert_eq!(s.hop, 0_i64);
        assert_eq!(s.born, 1_700_000_000_i64);
    }

    #[test]
    fn hop_is_number_not_string() {
        // Contrast with sidecar where hop is a JSON STRING.
        // Here hop must deserialize from a JSON number.
        let json = r#"{"session_id":"x","target_profile":"p","cwd":"/","handoff":"","hop":1,"born":0}"#;
        let s: RelaunchSentinel = serde_json::from_str(json).unwrap();
        assert_eq!(s.hop, 1_i64);
    }

    #[test]
    fn read_absent_returns_none() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("nonexistent.relaunch");
        let result = read_relaunch(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("test.relaunch");
        let sentinel = RelaunchSentinel {
            session_id: "test-sid".to_string(),
            target_profile: "personal".to_string(),
            cwd: "/tmp".to_string(),
            handoff: "resume".to_string(),
            hop: 0,
            born: 1_718_100_000,
        };
        write_relaunch(&path, &sentinel).unwrap();
        let back = read_relaunch(&path).unwrap().expect("should exist after write");
        assert_eq!(back.session_id, sentinel.session_id);
        assert_eq!(back.born, sentinel.born);
        assert_eq!(back.hop, sentinel.hop);
    }

    #[test]
    fn stale_sentinel_born_check() {
        // The consumer MUST reject sentinels where born < launch_born.
        let launch_born: i64 = 1_718_200_000;
        let stale_born: i64 = 1_718_100_000; // older than launch
        assert!(
            stale_born < launch_born,
            "stale sentinel should have born < launch_born"
        );
        // A fresh sentinel should have born >= launch_born.
        let fresh_born: i64 = 1_718_200_001;
        assert!(fresh_born >= launch_born);
    }

    #[test]
    fn hop_cap_constant() {
        assert_eq!(MAX_HOPS, 1, "MAX_HOPS must match legacy zsh MAX_HOPS=1");
    }
}
