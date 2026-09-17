//! `csm hook` — Claude Code Stop/SubagentStop/SessionEnd hook handler.
//!
//! Invoked by Claude Code as a hook process with the event JSON on stdin.
//! This is the **`csm hook` subcommand** — there is no separate `csm-hook` binary.
//!
//! Commit ordering (matches the legacy shell implementation):
//!   1. merge-sidecar hop
//!   2. write `.relaunch` sentinel (atomic tmp+rename)
//!   3. noclobber-create `.switched` marker
//!   4. re-stamp `.last-switch`
//!   5. write `<sid>.stop` flag (Windows) / `kill(pid, SIGTERM)` (POSIX)
//!      — stop is LAST: supervisor must see a complete sentinel before being asked to stop.
//!
//! The statusline entry point ([`run_from_statusline`]) claims `.switched`
//! *before* committing ([`stop::claim_switched`], see below), so on that path
//! the on-disk order is `.switched` → `.relaunch` → `.last-switch`
//! (confirmed against a live limit switch).
//!
//! `--owner <dir>` is the CLAUDE_CONFIG_DIR of the profile that owns this hook instance.
//! It is baked into the per-profile shim deployed outside this crate; the hook uses it
//! to locate the correct profile context.
//!
//! # Second entry point: the statusline tick
//!
//! [`run_from_statusline`] runs the same classification off `csm usage
//! capture` (and the capture inside `csm statusline`). It exists because a
//! subscription cap does not produce a hook event at all: when a request
//! fails with a 429 that carries a reset time, Claude Code (observed on
//! 2.1.270) neither ends the turn with `StopFailure` nor fires `Stop` — it
//! shows "Weekly limit reached · Retrying in 6h" and parks the turn in an
//! internal auto-retry wait, indefinitely. The only csm code that still runs
//! while the turn is parked is the statusLine command, which Claude Code keeps
//! invoking about once a second with the live `rate_limits` for the session's
//! account. So the tick is where the switch has to happen. Its stdout is not a
//! terminal (the statusLine wrapper discards it), so this path never emits the
//! OSC 777 notification — the relaunched session's handoff prompt is the
//! user-visible signal.

pub mod detect;
pub mod notify;
pub mod stop;

use std::path::Path;

use anyhow::Context as _;

/// Read the `hop` field from `<sid>.json` sidecar, tolerating both String and
/// Number forms. Returns 0 on missing/corrupt sidecar (the legacy zsh wrote
/// hop as a JSON string; readers accept both forms). Delegates to the single
/// `Sidecar::hop_int` SSOT so the String/Number tolerance rule lives in
/// exactly one place — this was previously duplicated byte-for-byte in
/// `detect.rs` and `stop.rs`.
pub(crate) fn read_sidecar_hop(sid: &str) -> i64 {
    crate::sidecar::read_sidecar(&crate::paths::sidecar(sid))
        .map(|s| s.hop_int())
        .unwrap_or(0)
}

/// First 8 bytes of a session UUID, for compact log lines and handoff
/// prompts. Panic-free on any input (unlike a raw `&sid[..8]` slice, which
/// panics on a session id shorter than 8 bytes).
pub(crate) fn sid_short(sid: &str) -> &str {
    sid.get(..8).unwrap_or(sid)
}

/// Build one `limit-switch.log` line. `kind` is `"notify-only"` or
/// `"limit-switch"`; `detail` carries the kind-specific fields (`msg=…`, or
/// `to=… cwd=… born=…`); `via` is `Some("statusline")` for the statusline
/// entry point and `None` for the hook entry point (whose lines carry no
/// `via=` suffix, matching the format before this helper existed).
fn decision_log_line(kind: &str, sid_short: &str, detail: &str, via: Option<&str>) -> String {
    match via {
        Some(via) => format!("{kind} sid={sid_short} {detail} via={via}"),
        None => format!("{kind} sid={sid_short} {detail}"),
    }
}

/// Entry point for `csm hook [--owner <profile_dir>]`.
///
/// `owner_dir` is the profile directory (value of CLAUDE_CONFIG_DIR for the hook's
/// owning profile). It is used to resolve profile context when needed. The hook
/// reads event JSON from stdin and, depending on the detected limit state,
/// writes the relaunch sentinel and signals the supervisor to stop.
pub fn run(owner_dir: &Path) -> anyhow::Result<()> {
    // Parse hook input from stdin.
    let input = detect::parse_stdin().context("failed to parse hook stdin JSON")?;
    run_with_input(owner_dir, input)
}

/// The body of [`run`] after stdin has been parsed into a [`detect::HookInput`].
/// Split out so tests can drive it with a synthetic input instead of the
/// process's real stdin (`run` itself blocks on `detect::parse_stdin()` when
/// stdin is an interactive terminal or a pipe that never closes).
pub(crate) fn run_with_input(owner_dir: &Path, input: detect::HookInput) -> anyhow::Result<()> {
    // session_id is required — exit 0 silently if missing (hook contract).
    let sid = match &input.session_id {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            // No session_id — exit cleanly; hook contract says exit 0.
            return Ok(());
        }
    };

    // Classify the hook event and determine whether a limit-switch is warranted.
    // classify() reproduces the full legacy shell implementation's flow including
    // kill-switches, reason gate, detection tiers, managed-session gate, cooldown,
    // and hop guard.
    let decision = detect::classify(&input, owner_dir)?;

    match decision {
        detect::Decision::Skip => {
            // Nothing to do — a kill-switch, cooldown, marker, or no-limit result.
        }

        detect::Decision::NotifyOnly { ref message } => {
            // Notify-only: user-quit + limited, no-target, detect-only mode, or
            // unmanaged session. Emit OSC 777 notify on stdout.
            // Log goes to the smart_dir limit-switch.log.
            let log_msg = decision_log_line(
                "notify-only",
                sid_short(&sid),
                &format!("msg={message}"),
                None,
            );
            notify::emit_osc777(message).unwrap_or(()); // best-effort stdout
            let _ = notify::append_log(&sid, &log_msg); // best-effort log
        }

        detect::Decision::LimitSwitch {
            ref message,
            ref target_profile,
            ref handoff,
            ref cwd,
            born,
            dimension: _,
        } => {
            // Full limit-switch commit sequence (matches the legacy shell
            // implementation's ordering): notify first (stdout before any
            // mutation), then commit_and_stop.
            notify::emit_osc777(message).unwrap_or(());

            let log_msg = decision_log_line(
                "limit-switch",
                sid_short(&sid),
                &format!("to={target_profile} cwd={cwd} born={born}"),
                None,
            );
            let _ = notify::append_log(&sid, &log_msg);

            stop::commit_and_stop(sid.as_str(), target_profile, handoff, cwd, born)
                .with_context(|| format!("commit_and_stop failed for session {sid}"))?;
        }
    }

    Ok(())
}

/// Entry point for the statusline tick. `raw` is the statusLine stdin JSON
/// exactly as `csm usage capture` read it; `capture` is what
/// [`crate::usage::local::record_statusline_payload`] just made of it.
///
/// Order of checks, cheapest first, because this runs about once a second
/// for every live session:
///
/// 1. [`detect::statusline_limit`] over the merged reading — pure, no I/O.
///    Almost every tick ends here.
/// 2. Parse `raw` as a [`detect::HookInput`] (statusLine stdin carries the
///    same `session_id`/`cwd`/`transcript_path` keys a hook event does).
/// 3. [`detect::classify_with`] with the reading as a definitive live limit.
///    Kill-switches, `.switched`, target pick, relaunch switch, managed gate,
///    cooldown exception and hop guard all apply exactly as for the hook.
/// 4. On `LimitSwitch`, claim `.switched` first ([`stop::claim_switched`] —
///    ticks overlap; only one may commit), then [`stop::commit_and_stop`].
///    If the commit fails the claim is released so the next tick retries.
///
/// Never returns an error and never writes to stdout/stderr: the capture
/// this rides on is fire-and-forget and must stay that way. Outcomes are
/// logged to `limit-switch.log` with `via=statusline`.
pub fn run_from_statusline(raw: &str, capture: &crate::usage::local::StatuslineCapture) {
    let Some(limit_msg) = detect::statusline_limit(&capture.usage) else {
        return;
    };
    let Ok(input) = detect::parse_input(raw) else {
        return;
    };
    let Some(sid) = input.session_id.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let owner_dir = Path::new(&capture.profile_dir);
    let Ok(decision) = detect::classify_with(&input, owner_dir, Some(&limit_msg)) else {
        return;
    };
    let sid_short = sid_short(&sid);

    match decision {
        detect::Decision::Skip => {}

        detect::Decision::NotifyOnly { ref message } => {
            // Deduped by `.detected` inside classify, so this lands once per
            // session, not once per second.
            let log_msg = decision_log_line(
                "notify-only",
                sid_short,
                &format!("msg={message}"),
                Some("statusline"),
            );
            let _ = notify::append_log(&sid, &log_msg);
        }

        detect::Decision::LimitSwitch {
            message: _,
            ref target_profile,
            ref handoff,
            ref cwd,
            born,
            dimension: _,
        } => {
            if !stop::claim_switched(&sid) {
                return;
            }
            let log_msg = decision_log_line(
                "limit-switch",
                sid_short,
                &format!("to={target_profile} cwd={cwd} born={born}"),
                Some("statusline"),
            );
            let _ = notify::append_log(&sid, &log_msg);

            if let Err(e) = stop::commit_and_stop(sid.as_str(), target_profile, handoff, cwd, born)
            {
                let _ = notify::append_log(
                    &sid,
                    &decision_log_line(
                        "limit-switch",
                        sid_short,
                        &format!("commit failed: {e:#}"),
                        Some("statusline"),
                    ),
                );
                let _ = std::fs::remove_file(crate::paths::switched(&sid));
            }
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────
//
// `run` and `run_from_statusline` are the two process entry points; every
// other test in this crate exercises the pure decision core
// (`detect::classify_with`) directly. These tests instead drive the entry
// points themselves, through the real env-var seams (`HOME` for
// `paths::smart_dir()`, `CSM_USAGE_CMD` for the target-pick, and
// `CLAUDE_SMART_CLAUDE_BIN` for the managed-session PID check), so a
// regression in the glue between `classify_with` and its callers (the claim/
// release ordering, the log-line `via` suffix, the early-return guards) shows
// up here even though each piece it calls is separately unit-tested.
//
// No `actions_for(decision) -> Vec<Action>` unification: `run` and
// `run_from_statusline` have materially different, ordering-sensitive
// sequences (`claim_switched`/release-on-failure exists only in the
// statusline path), so each gets its own direct coverage instead.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::local::StatuslineCapture;
    use crate::usage::model::{ProfileUsage, UsageData, UsageSection};
    use std::collections::HashMap;
    use std::process::{Child, Command};

    /// Everything one test needs torn down: restores `HOME`/`CSM_USAGE_CMD`/
    /// `CLAUDE_SMART_CLAUDE_BIN` and kills the fake managed process (if any)
    /// on drop, so a panicking assertion never leaks state into the next
    /// test even though the caller's lock guards are dropped right along
    /// with it.
    struct EnvFixture {
        home: tempfile::TempDir,
        prev_usage_cmd: Option<std::ffi::OsString>,
        prev_launch_bin: Option<std::ffi::OsString>,
        fake_proc: Option<Child>,
    }

    impl Drop for EnvFixture {
        fn drop(&mut self) {
            if let Some(mut child) = self.fake_proc.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            crate::testenv::set_test_home(None);
            match self.prev_usage_cmd.take() {
                Some(v) => crate::testenv::set_var("CSM_USAGE_CMD", &v.to_string_lossy()),
                None => crate::testenv::remove_var("CSM_USAGE_CMD"),
            }
            match self.prev_launch_bin.take() {
                Some(v) => crate::testenv::set_var("CLAUDE_SMART_CLAUDE_BIN", &v.to_string_lossy()),
                None => crate::testenv::remove_var("CLAUDE_SMART_CLAUDE_BIN"),
            }
        }
    }

    /// Point the resolved home dir (and so `paths::smart_dir()`) at a fresh
    /// temp dir, and wire `CSM_USAGE_CMD` to hand `usage::fetch()` the given
    /// `UsageData` verbatim (via `cat <tmpfile>`) instead of touching any real
    /// profile. Caller must hold `lock_for("CSM_USAGE_CMD")` and
    /// `lock_for("CLAUDE_SMART_CLAUDE_BIN")` for the fixture's whole
    /// lifetime — those are real process-global env vars; the home-dir
    /// override itself is thread-local and needs no lock.
    fn isolated_env(usage: &UsageData) -> EnvFixture {
        let home = tempfile::tempdir().expect("tempdir");
        let usage_json = serde_json::to_string(usage).expect("serialize UsageData");
        let usage_file = home.path().join("usage-cmd.json");
        std::fs::write(&usage_file, &usage_json).expect("write usage fixture");

        let prev_usage_cmd = std::env::var_os("CSM_USAGE_CMD");
        let prev_launch_bin = std::env::var_os("CLAUDE_SMART_CLAUDE_BIN");

        crate::testenv::set_test_home(Some(home.path().to_path_buf()));
        crate::testenv::set_var("CSM_USAGE_CMD", &format!("cat {}", usage_file.display()));
        crate::testenv::remove_var("CLAUDE_SMART_CLAUDE_BIN");

        EnvFixture {
            home,
            prev_usage_cmd,
            prev_launch_bin,
            fake_proc: None,
        }
    }

    /// A `UsageData` with one viable target profile ("healthy", week_all 10%)
    /// and no entry for the current profile at all — `pick_target` excludes
    /// the current profile by name regardless, so its absence here is
    /// equivalent to "saturated" for the purpose of these tests.
    fn usage_with_one_viable_target() -> UsageData {
        let mut profiles = HashMap::new();
        profiles.insert(
            "healthy".to_string(),
            ProfileUsage {
                week_all: Some(UsageSection {
                    pct: 10,
                    resets: None,
                    resets_at: None,
                }),
                ..Default::default()
            },
        );
        UsageData {
            captured_at: None,
            profiles,
            errors: None,
            ..Default::default()
        }
    }

    /// A `UsageData` with no viable candidates at all (empty profile map) —
    /// `pick_target` returns `None` regardless of the current profile.
    fn usage_with_no_viable_target() -> UsageData {
        UsageData {
            captured_at: None,
            profiles: HashMap::new(),
            errors: None,
            ..Default::default()
        }
    }

    /// A `StatuslineCapture` whose merged reading trips `statusline_limit`
    /// (week_all at/above `CLAUDE_LIMIT_PCT`) for `profile_dir`.
    fn capped_capture(profile_dir: &str) -> StatuslineCapture {
        StatuslineCapture {
            profile_dir: profile_dir.to_string(),
            usage: ProfileUsage {
                week_all: Some(UsageSection {
                    pct: 100,
                    resets: None,
                    resets_at: None,
                }),
                ..Default::default()
            },
        }
    }

    /// A `StatuslineCapture` whose reading is healthy on every dimension —
    /// `statusline_limit` must return `None` for it.
    fn healthy_capture(profile_dir: &str) -> StatuslineCapture {
        StatuslineCapture {
            profile_dir: profile_dir.to_string(),
            usage: ProfileUsage {
                session: Some(UsageSection {
                    pct: 21,
                    resets: None,
                    resets_at: None,
                }),
                week_all: Some(UsageSection {
                    pct: 40,
                    resets: None,
                    resets_at: None,
                }),
                ..Default::default()
            },
        }
    }

    /// Spawn a real, harmless `sleep` process and register it (via
    /// `CLAUDE_SMART_CLAUDE_BIN=sleep`) as "managed" so `managed_session`'s
    /// live-process check passes without needing an actual `claude`/`node`
    /// binary on the test host. Writes `<sid>.pid` under `home`'s smart_dir
    /// and stores the child on `fixture` so it is reaped on drop.
    fn spawn_fake_managed_process(fixture: &mut EnvFixture, sid: &str) {
        crate::testenv::set_var("CLAUDE_SMART_CLAUDE_BIN", "sleep");
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn fake managed process");
        let pid = child.pid();
        // Ride out the post-spawn exec window on Linux; see
        // `proc_check::wait_until_live_claude_or_node`'s doc comment.
        assert!(
            crate::platform::proc_check::wait_until_live_claude_or_node(
                pid,
                std::time::Duration::from_secs(5)
            ),
            "fake managed process must become recognizable as live"
        );
        fixture.fake_proc = Some(child);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        std::fs::write(smart_dir.join(format!("{sid}.pid")), format!("{pid} 1000"))
            .expect("write pid file");
    }

    trait ChildPid {
        fn pid(&self) -> u32;
    }
    impl ChildPid for Child {
        fn pid(&self) -> u32 {
            std::process::Child::id(self)
        }
    }

    // ── run_from_statusline: early-return guards ──────────────────────────────

    #[test]
    fn run_from_statusline_skips_when_statusline_limit_is_none() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let fixture = isolated_env(&usage_with_no_viable_target());
        let capture = healthy_capture("/Users/example/.claude.home");

        run_from_statusline(r#"{"session_id": "sid-healthy-0001"}"#, &capture);

        // Nothing should have touched smart_dir at all — the function must
        // return before any I/O when the merged reading is under threshold.
        assert!(
            !fixture
                .home
                .path()
                .join(".claude.shared")
                .join("smart")
                .exists(),
            "smart_dir must not be created when statusline_limit is None"
        );
    }

    #[test]
    fn run_from_statusline_skips_on_unparseable_raw() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let fixture = isolated_env(&usage_with_no_viable_target());
        let capture = capped_capture("/Users/example/.claude.home");

        run_from_statusline("{not json", &capture);

        assert!(
            !fixture
                .home
                .path()
                .join(".claude.shared")
                .join("smart")
                .exists(),
            "smart_dir must not be created when raw stdin fails to parse"
        );
    }

    #[test]
    fn run_from_statusline_skips_on_missing_session_id() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let fixture = isolated_env(&usage_with_no_viable_target());
        let capture = capped_capture("/Users/example/.claude.home");

        // Valid JSON, but no "session_id" key at all.
        run_from_statusline(r#"{"cwd": "/Users/example/Projects/foo"}"#, &capture);

        assert!(
            !fixture
                .home
                .path()
                .join(".claude.shared")
                .join("smart")
                .exists(),
            "smart_dir must not be created when session_id is missing"
        );
    }

    // ── run_from_statusline: NotifyOnly logs exactly once, via=statusline ─────

    #[test]
    fn run_from_statusline_notify_only_appends_one_log_line_with_via_suffix() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let fixture = isolated_env(&usage_with_no_viable_target());
        let capture = capped_capture("/Users/example/.claude.limited");
        let sid = "sid-notify-only-0001";

        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let log_path = fixture
            .home
            .path()
            .join(".claude.shared")
            .join("smart")
            .join("limit-switch.log");
        let content = std::fs::read_to_string(&log_path).expect("limit-switch.log written");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "expected exactly one log line: {content:?}");
        assert!(lines[0].contains("notify-only"), "line: {}", lines[0]);
        assert!(
            lines[0].contains(&format!("sid={}", sid_short(sid))),
            "line: {}",
            lines[0]
        );
        assert!(lines[0].contains("via=statusline"), "line: {}", lines[0]);
    }

    // ── run_from_statusline: claim/release around commit_and_stop ─────────────

    #[test]
    fn run_from_statusline_releases_claim_on_commit_failure() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_one_viable_target());
        let sid = "sid-commit-fails-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        // Force `commit_and_stop`'s `write_relaunch` step to fail: pre-create
        // its target path as a directory, so the atomic tmp+rename onto it
        // errors instead of replacing a file.
        let relaunch_dir = fixture
            .home
            .path()
            .join(".claude.shared")
            .join("smart")
            .join(format!("{sid}.relaunch"));
        std::fs::create_dir_all(&relaunch_dir).expect("pre-create .relaunch as a directory");

        let capture = capped_capture("/Users/example/.claude.limited");
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let switched_path = fixture
            .home
            .path()
            .join(".claude.shared")
            .join("smart")
            .join(format!("{sid}.switched"));
        assert!(
            !switched_path.exists(),
            "a failed commit must release the .switched claim so the next tick retries"
        );
    }

    // ── run: the hook contract's silent exit 0 ─────────────────────────────────

    #[test]
    fn run_exits_ok_without_session_id() {
        // `run` itself reads real process stdin via `detect::parse_stdin()`,
        // which blocks forever under a non-EOF stdin (an interactive
        // terminal, or a pipe that never closes) — so this test drives
        // `run_with_input` directly with the empty-input `HookInput`
        // (`detect::parse_input("")` parses blank input to all-`None`
        // fields), exactly the "no session_id" case the hook contract
        // requires to exit 0 silently, with no smart_dir I/O at all.
        let home = tempfile::tempdir().unwrap();
        let result = crate::testenv::with_test_home(home.path(), || {
            run_with_input(
                Path::new("/Users/example/.claude.home"),
                detect::parse_input("").unwrap(),
            )
        });

        assert!(
            result.is_ok(),
            "hook contract: missing session_id exits Ok(())"
        );
    }
}
