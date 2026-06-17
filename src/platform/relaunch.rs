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
/// **Phase 0 stub** — the full implementation is Phase 9 in the scaffold §6.
pub fn run_relaunch_loop(
    _launcher: &dyn crate::platform::launcher::Launcher,
    _spec: &LaunchSpec,
) -> anyhow::Result<()> {
    unimplemented!(
        "run_relaunch_loop: \
         clobber guard → launch → born-check → consume → hop-cap → build-next-CLI → loop \
         (Phase 0 stub — implement in Phase 9)"
    )
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
    /// Working directory to start claude in.
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
