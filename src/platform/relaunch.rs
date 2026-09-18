//! Relaunch loop and the `RelaunchSentinel` serde model.
//!
//! The relaunch loop (`relaunch_loop`) is platform-agnostic and consumes a
//! `&dyn Launcher` + `ProcCheck`.  The public entry point `run_relaunch_loop`
//! gates it per platform: on unix it runs the full loop; on Windows it falls
//! back to a single launch-without-relaunch (`run_once`) because the Windows
//! console-stop path has two BLOCKING empirical checks that are not yet
//! verified (interactive Ctrl-C forwarding + CTRL_BREAK transcript flush — see
//! `platform/windows.rs` and CLAUDE.md's "Known gaps" section).  Shipping the
//! relaunch loop on Windows before those pass risks losing the supervisor
//! (Ctrl-C kills it) or truncating the session transcript on a limit switch.
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
/// Field names match the legacy zsh `write_relaunch` jq output exactly —
/// external readers of `<sid>.relaunch` depend on this: `session_id`,
/// `target_profile`, `cwd`, `handoff`, `hop`, `born`.
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
    /// `Some(model)` for a same-account model fallback (a `week_fable` cap —
    /// see `crate::hook::detect::fable_fallback_model`): the hop resumes the
    /// SAME session on this model instead of switching profiles.  `None` is
    /// an ordinary account switch, the pre-C44 shape.  `#[serde(default)]` so
    /// an older sentinel written before this field existed still reads back
    /// as `None`, never a parse error (rollback safety, same contract as
    /// every other field here).
    #[serde(default)]
    pub model_override: Option<String>,
}

/// Maximum number of limit-switch hops before the relaunch loop breaks.
/// Matches the legacy zsh `MAX_HOPS=1` constant.
// Only the unix relaunch loop reads this; on Windows the loop is gated off
// (`run_relaunch_loop` → `run_once`), so the const is unused in the Windows bin
// build (still exercised by the cfg(test) suite). Kept for when the Windows loop
// is ungated.
#[cfg_attr(windows, allow(dead_code))]
pub const MAX_HOPS: i64 = 1;

/// Read `<sid>.relaunch` from `path`.  Returns `None` if the file is absent;
/// propagates I/O or parse errors.
// Unused in the Windows bin build (relaunch loop gated off); see `MAX_HOPS`.
#[cfg_attr(windows, allow(dead_code))]
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
    // Clean up the tmp file if the atomic rename fails (e.g. cross-filesystem),
    // so a failed write never leaves a stale .relaunch.tmp on disk.
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Entry point for the foreground relaunch loop (unix).
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
#[cfg(not(windows))]
pub fn run_relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    relaunch_loop(launcher, spec)
}

/// Entry point on Windows — the console-stop relaunch path is gated OFF until its
/// two BLOCKING empirical checks pass (see module doc + `platform/windows.rs`).
/// Falls back to a single launch with no relaunch so an unverified supervisor can
/// never eat an interactive Ctrl-C or truncate the transcript on a switch.
#[cfg(windows)]
pub fn run_relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    run_once(launcher, spec)
}

/// Single launch with no relaunch handling — the Windows fall-back while the
/// console-stop relaunch loop is gated off. Spawns claude once in the foreground,
/// cleans up `<sid>.pid`, and propagates the child's exit code. Any `.relaunch`
/// sentinel the hook may have written is intentionally ignored (no relaunch).
#[cfg(windows)]
fn run_once(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    use crate::paths;

    let sid = spec.session_id.clone();
    let pid_path = paths::pid_file(&sid);

    // Ensure the launch profile is provisioned (no-op on non-unix, where the
    // symlink is handled OS-side). Idempotent + best-effort.
    crate::provision::ensure_provisioned_soft(&spec.profile_dir);

    let mut env: HashMap<OsString, OsString> = HashMap::new();
    env.insert(
        OsString::from("CLAUDE_CONFIG_DIR"),
        spec.profile_dir.clone().into_os_string(),
    );

    let (status, _handle) = launcher.run_foreground(&sid, &spec.cli, &env)?;
    let _ = std::fs::remove_file(&pid_path);
    exit_with(status)
}

/// The full platform-agnostic relaunch loop (used on unix; gated off on Windows,
/// where `run_relaunch_loop` dispatches to `run_once` instead, so this is not
/// compiled there).
#[cfg(not(windows))]
fn relaunch_loop(
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

        // Ensure this hop's profile satisfies the provisioning invariants
        // (dir exists, plugins → shared SSOT) BEFORE launching claude under it.
        // Idempotent + best-effort: a hiccup must not block the relaunch.
        crate::provision::ensure_provisioned_soft(&profile_dir);

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
            eprintln!("csm: limit-switch hop cap ({MAX_HOPS}) reached — not relaunching again");
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

        // Build the next iteration's CLI: same sid, resume the session, re-apply
        // the launch flags the sidecar remembers, and inject the handoff prompt
        // (unless suppressed).
        let remembered = crate::sidecar::read_sidecar(&paths::sidecar(&sid)).unwrap_or_default();
        let (next_cli, dropped) = build_next_cli(&sid, &sentinel, &remembered);
        if !dropped.is_empty() {
            // The switch changed the session's argv. Say so where every other
            // limit-switch event is recorded, so a session that comes back
            // without something it was launched with is explainable.
            let _ = crate::hook::notify::append_log(
                &sid,
                &format!(
                    "relaunch sid={} dropped passthru: {}",
                    crate::hook::sid_short(&sid),
                    crate::cli::carry::describe_dropped(&dropped)
                ),
            );
        }
        cli = next_cli;
    }
}

/// Build the claude CLI for the next relaunch hop: resume the same session,
/// re-apply the `--permission-mode`/`--effort`/`--model` the sidecar remembers
/// for it, replay the session-shaping flags of the original launch, and pass
/// the handoff prompt (if any). Returns the argv and the remembered passthru
/// tokens it could not replay, which the caller logs.
///
/// The remembered flags matter because a switch is meant to continue the
/// same work: a session launched as `csm --model <m>` that moves to another
/// profile must come back up on `<m>`, not on that profile's default model.
/// The same holds for the flags `csm run` never consumed and forwarded to
/// claude — a session launched `--dangerously-skip-permissions --add-dir /x`
/// that comes back without them stops on the first permission prompt with
/// nobody there to answer it. `csm run` persists both into the sidecar at
/// launch precisely so this hop can read them back. What is *not* replayed is
/// the initial prompt (`--resume` already carries that conversation) and every
/// flag that would fight this argv or the resume itself; `cli::carry` owns
/// that decision.
///
/// Only the unix relaunch loop calls this; on Windows the loop is gated off
/// (`run_relaunch_loop` → `run_once`, which relaunches nothing and so has no
/// second argv to build), but the builder still compiles there so both
/// platforms would carry identically the day that gate lifts.
#[cfg_attr(windows, allow(dead_code))]
fn build_next_cli(
    sid: &str,
    sentinel: &RelaunchSentinel,
    remembered: &crate::sidecar::Sidecar,
) -> (Vec<OsString>, Vec<String>) {
    // A same-account model fallback (`sentinel.model_override`, see
    // `crate::hook::detect::fable_fallback_model`) overrides the remembered
    // model for this hop only — clone-and-replace, never a sidecar rewrite,
    // so a LATER hop that carries no override still replays whatever model
    // the session actually launched with. `sidecar_flags()` (below) is the
    // FIRST thing appended after the session id, ahead of all carried
    // passthru, so `--model` always lands in the same argv position whether
    // it came from the sidecar or from this override.
    let remembered = match &sentinel.model_override {
        Some(m) => {
            let mut c = remembered.clone();
            c.model = Some(m.clone());
            c
        }
        None => remembered.clone(),
    };

    let mut cli: Vec<OsString> = Vec::new();
    cli.push(OsString::from("--resume"));
    cli.push(OsString::from(sid));
    cli.extend(remembered.sidecar_flags());

    let passthru = remembered.passthru.as_deref().unwrap_or(&[]);
    let carried = crate::cli::carry::carry_passthru(passthru);
    let open_variadic = carried.trailing_variadic;
    cli.extend(carried.carried);

    // The handoff prompt is the first turn after resume (e.g. "resume"). Empty =
    // suppressed (user already had a pending tail); pass nothing then.
    if !sentinel.handoff.is_empty() {
        // A carried run that ends inside a variadic flag would swallow the
        // prompt as one more of its values, and the session would come back
        // with nothing to do. `--` closes the run; it is emitted only in that
        // case, so an argv that was already correct keeps its exact shape.
        if open_variadic {
            cli.push(OsString::from("--"));
        }
        cli.push(OsString::from(&sentinel.handoff));
    }
    (cli, carried.dropped)
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

    fn sentinel(handoff: &str) -> RelaunchSentinel {
        RelaunchSentinel {
            session_id: "abc123".to_string(),
            target_profile: "home".to_string(),
            cwd: "/home/you/projects".to_string(),
            handoff: handoff.to_string(),
            hop: 1,
            born: 1,
            model_override: None,
        }
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// The hop re-applies the sidecar's remembered launch flags between the
    /// resume verb and the handoff prompt, so the switched session runs the
    /// same model/effort/permission mode the user launched with.
    #[test]
    fn build_next_cli_carries_remembered_flags() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            effort: Some("high".to_string()),
            permission_mode: Some("plan".to_string()),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--permission-mode",
                "plan",
                "--effort",
                "high",
                "--model",
                "some-model",
                "carry on",
            ]
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn build_next_cli_without_flags_is_resume_and_handoff() {
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &Default::default());
        assert_eq!(strs(&cli), ["--resume", "abc123", "carry on"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn build_next_cli_empty_handoff_passes_no_prompt() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel(""), &remembered);
        assert_eq!(strs(&cli), ["--resume", "abc123", "--model", "some-model"]);
    }

    /// The whole point of remembering the passthru: the switched session comes
    /// back with the permission bypass and the extra directory it was launched
    /// with, ordered after the sidecar flags and before the handoff prompt.
    #[test]
    fn build_next_cli_carries_remembered_passthru_before_the_handoff() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            passthru: Some(vec![
                "--dangerously-skip-permissions".to_string(),
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
                "--settings".to_string(),
                "/Users/example/s.json".to_string(),
                "do the thing".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--model",
                "some-model",
                "--dangerously-skip-permissions",
                "--add-dir",
                "/Users/example/x",
                "--settings",
                "/Users/example/s.json",
                "carry on",
            ]
        );
        assert_eq!(
            dropped,
            ["do the thing"],
            "the initial prompt is not replayed; the caller logs that it went"
        );
    }

    #[test]
    fn build_next_cli_with_a_prompt_only_passthru_carries_nothing() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec!["do the thing".to_string()]),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(strs(&cli), ["--resume", "abc123", "carry on"]);
        assert_eq!(dropped, ["do the thing"]);
    }

    /// The regression this guards: a launch whose last carried flag is
    /// variadic (`--add-dir /x`) used to hand claude a prompt it would read as
    /// one more directory, so the switched session came back with the extra
    /// directory wrong AND no first turn. `--` closes the run.
    #[test]
    fn build_next_cli_closes_a_trailing_variadic_before_the_handoff() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--add-dir",
                "/Users/example/x",
                "--",
                "carry on",
            ]
        );
    }

    /// The separator is not free: it changes how claude reads everything after
    /// it, so it appears only when a variadic run is actually open. A launch
    /// whose carried tail is a boolean (or a one-value flag with its value)
    /// keeps the argv it always had.
    #[test]
    fn build_next_cli_omits_the_separator_when_nothing_can_absorb() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--add-dir",
                "/Users/example/x",
                "--dangerously-skip-permissions",
                "carry on",
            ],
            "the boolean closed the variadic run already"
        );
    }

    #[test]
    fn build_next_cli_with_an_open_variadic_and_no_handoff_adds_no_separator() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel(""), &remembered);
        assert_eq!(
            strs(&cli),
            ["--resume", "abc123", "--add-dir", "/Users/example/x"],
            "nothing follows the run, so there is nothing to separate"
        );
    }

    /// A same-account model fallback (`sentinel.model_override`) overrides
    /// the remembered model for this hop — the resumed session gets exactly
    /// one `--model` flag, the fallback model, not the sidecar's original one.
    #[test]
    fn sentinel_model_override_emits_exactly_one_model_flag() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            effort: Some("high".to_string()),
            ..Default::default()
        };
        let mut s = sentinel("carry on");
        s.model_override = Some("opus".to_string());
        let (cli, dropped) = build_next_cli("abc123", &s, &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume", "abc123", "--effort", "high", "--model", "opus", "carry on",
            ]
        );
        assert_eq!(
            cli.iter().filter(|a| *a == "--model").count(),
            1,
            "exactly one --model flag, never both the sidecar's and the override's"
        );
        assert!(dropped.is_empty());
    }

    /// No `model_override` (the ordinary account-switch sentinel, `None`) —
    /// the hop replays whatever model the sidecar remembers, exactly as
    /// before this field existed.
    #[test]
    fn sentinel_without_model_override_replays_sidecar_model() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            ["--resume", "abc123", "--model", "some-model", "carry on"]
        );
    }

    /// Rollback safety: a `.relaunch` written by an OLDER binary (no
    /// `model_override` key at all) must still parse, with the field reading
    /// back as `None` — the same `#[serde(default)]` contract as every other
    /// forward-compat field on this type. Mirrors
    /// `read_compat_unknown_future_field_is_ignored_not_fatal`, but for a
    /// field THIS version knows and an OLDER file lacks, not the reverse.
    #[test]
    fn read_compat_old_file_without_model_override_defaults_to_none() {
        let json = r#"{
            "session_id":"sid-9","target_profile":"work","cwd":"/tmp",
            "handoff":"resume","hop":1,"born":1700000000
        }"#;
        let s: RelaunchSentinel =
            serde_json::from_str(json).expect("a sentinel without model_override must still parse");
        assert_eq!(s.model_override, None);
    }

    #[test]
    fn roundtrip_sentinel() {
        let sentinel = RelaunchSentinel {
            session_id: "abc123".to_string(),
            target_profile: "home".to_string(),
            cwd: "/home/you/projects".to_string(),
            handoff: "resume".to_string(),
            hop: 1,
            born: 1_718_000_000,
            model_override: None,
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
    fn read_compat_unknown_future_field_is_ignored_not_fatal() {
        // Rollback safety: a NEWER binary may write an extra field this version
        // doesn't know. Reading it must succeed (the sentinel is consume-and-
        // delete, so dropping the unknown field loses nothing) and the known
        // fields must still be correct — never a parse abort.
        let json = r#"{
            "session_id":"sid-9","target_profile":"work","cwd":"/tmp",
            "handoff":"resume","hop":1,"born":1700000000,
            "futureField":"ignored","another":{"x":1}
        }"#;
        let s: RelaunchSentinel = serde_json::from_str(json).expect("unknown field must not abort");
        assert_eq!(s.session_id, "sid-9");
        assert_eq!(s.target_profile, "work");
        assert_eq!(s.hop, 1_i64);
    }

    #[test]
    fn read_compat_corrupt_relaunch_errs_without_panic() {
        // A truncated/garbled .relaunch must surface as Err (the loop aborts the
        // relaunch and cleans up) — never a panic. Locks the failure mode.
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("corrupt.relaunch");
        std::fs::write(&path, "not json at all{{{").unwrap();
        let result = read_relaunch(&path);
        assert!(
            result.is_err(),
            "corrupt .relaunch must be Err, got {result:?}"
        );
    }

    #[test]
    fn hop_is_number_not_string() {
        // Contrast with sidecar where hop is a JSON STRING.
        // Here hop must deserialize from a JSON number.
        let json =
            r#"{"session_id":"x","target_profile":"p","cwd":"/","handoff":"","hop":1,"born":0}"#;
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
            target_profile: "home".to_string(),
            cwd: "/tmp".to_string(),
            handoff: "resume".to_string(),
            hop: 0,
            born: 1_718_100_000,
            model_override: None,
        };
        write_relaunch(&path, &sentinel).unwrap();
        let back = read_relaunch(&path)
            .unwrap()
            .expect("should exist after write");
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

    /// Cross-format contract: `Sidecar.hop` is written as a JSON STRING (source
    /// site: `merge_sidecar_hop` in `hook/stop.rs`, mirroring the legacy zsh
    /// `jq --arg` sidecar writer) while `RelaunchSentinel.hop` is written as a
    /// JSON NUMBER (source site: this struct's `pub hop: i64` field, matching
    /// the legacy zsh `write_relaunch` `jq --argjson` writer). Owner decision:
    /// the `.relaunch` number-vs-string read tolerance stays — this test pins
    /// what is *written*, not what is accepted on read.
    #[test]
    fn sidecar_hop_is_string_relaunch_hop_is_number() {
        let sidecar = crate::sidecar::Sidecar {
            hop: Some(serde_json::Value::String("2".to_string())),
            ..Default::default()
        };
        let sidecar_json = serde_json::to_value(&sidecar).unwrap();
        assert!(
            sidecar_json["hop"].is_string(),
            "sidecar hop must serialize as a JSON string, got {sidecar_json}"
        );

        let sentinel = RelaunchSentinel {
            session_id: "abc123".to_string(),
            target_profile: "home".to_string(),
            cwd: "/home/you/projects".to_string(),
            handoff: "resume".to_string(),
            hop: 2,
            born: 1,
            model_override: None,
        };
        let sentinel_json = serde_json::to_value(&sentinel).unwrap();
        assert!(
            sentinel_json["hop"].is_number(),
            "relaunch sentinel hop must serialize as a JSON number, got {sentinel_json}"
        );
    }
}
