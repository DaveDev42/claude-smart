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
            ref model_override,
        } => {
            // Full limit-switch commit sequence (matches the legacy shell
            // implementation's ordering): notify first (stdout before any
            // mutation), then commit_and_stop.
            notify::emit_osc777(message).unwrap_or(());

            // `target_profile == current_profile` for a fallback (5b), so the
            // detail records the model, not a `to=` switch, or limit-switch.log
            // would misread a same-account model change as an account switch.
            let detail = match model_override {
                Some(model) => {
                    format!("model-fallback={model} account={target_profile} cwd={cwd} born={born}")
                }
                None => format!("to={target_profile} cwd={cwd} born={born}"),
            };
            let log_msg = decision_log_line("limit-switch", sid_short(&sid), &detail, None);
            let _ = notify::append_log(&sid, &log_msg);

            stop::commit_and_stop(
                sid.as_str(),
                target_profile,
                handoff,
                cwd,
                born,
                model_override.as_deref(),
            )
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
/// 1. [`detect::statusline_limit_hit_default`] over the merged reading —
///    pure, no I/O. Almost every tick ends here. Its typed dimension threads
///    straight into step 3 rather than being re-parsed from the message.
/// 2. Parse `raw` as a [`detect::HookInput`] (statusLine stdin carries the
///    same `session_id`/`cwd`/`transcript_path` keys a hook event does).
/// 3. [`detect::classify_with`] with the hit as a definitive live limit.
///    Kill-switches, `.switched`, target pick, relaunch switch, managed gate,
///    cooldown exception and hop guard all apply exactly as for the hook.
/// 4. On `LimitSwitch`, claim the right one-shot marker first — `.switched`
///    via [`stop::claim_switched`] for an ordinary account switch,
///    `.model-fallback` via [`stop::claim_model_fallback`] for a same-account
///    model fallback (`model_override.is_some()`) — then
///    [`stop::commit_and_stop`]. Ticks overlap, so only one claimant may
///    commit; if the commit fails, the claim is released so the next tick
///    retries.
///
/// Never returns an error and never writes to stdout/stderr: the capture
/// this rides on is fire-and-forget and must stay that way. Outcomes are
/// logged to `limit-switch.log` with `via=statusline`.
pub fn run_from_statusline(raw: &str, capture: &crate::usage::local::StatuslineCapture) {
    let Some(limit_hit) = detect::statusline_limit_hit_default(&capture.usage) else {
        return;
    };
    let Ok(input) = detect::parse_input(raw) else {
        return;
    };
    let Some(sid) = input.session_id.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let owner_dir = Path::new(&capture.profile_dir);
    let Ok(decision) = detect::classify_with(&input, owner_dir, Some(&limit_hit)) else {
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
            ref model_override,
        } => {
            // Claim the right one-shot marker BEFORE committing, so two
            // overlapping ticks for the same session never both commit. A
            // model fallback claims `.model-fallback`, never `.switched` —
            // claiming `.switched` here would burn this session's
            // account-switch budget for a relaunch that never touched it,
            // blocking a real account switch this same session might still
            // need later (see `detect::fable_fallback_model`'s doc).
            let claimed = match model_override {
                Some(_) => stop::claim_model_fallback(&sid, target_profile),
                None => stop::claim_switched(&sid),
            };
            if !claimed {
                return;
            }
            // See run_with_input's identical comment: `target_profile ==
            // current_profile` for a fallback (5b), so the detail records the
            // model rather than a `to=` switch.
            let detail = match model_override {
                Some(model) => {
                    format!("model-fallback={model} account={target_profile} cwd={cwd} born={born}")
                }
                None => format!("to={target_profile} cwd={cwd} born={born}"),
            };
            let log_msg = decision_log_line("limit-switch", sid_short, &detail, Some("statusline"));
            let _ = notify::append_log(&sid, &log_msg);

            if let Err(e) = stop::commit_and_stop(
                sid.as_str(),
                target_profile,
                handoff,
                cwd,
                born,
                model_override.as_deref(),
            ) {
                let _ = notify::append_log(
                    &sid,
                    &decision_log_line(
                        "limit-switch",
                        sid_short,
                        &format!("commit failed: {e:#}"),
                        Some("statusline"),
                    ),
                );
                let release_path = match model_override {
                    Some(_) => crate::paths::model_fallback(&sid),
                    None => crate::paths::switched(&sid),
                };
                let _ = std::fs::remove_file(release_path);
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

    /// A `StatuslineCapture` whose merged reading trips `statusline_limit_hit`
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
    /// `statusline_limit_hit` must return `None` for it.
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

    /// A `StatuslineCapture` whose reading trips `statusline_limit_hit` on
    /// `week_fable` alone — session and week_all both healthy. Used by the
    /// fable-fallback tests: `run_from_statusline` must read this as a
    /// `LimitDimension::WeekFable` hit (threaded straight through from
    /// [`detect::statusline_limit_hit_default`]'s [`detect::LimitHit`]) and
    /// fall back to a model instead of picking a switch target.
    ///
    /// `week_fable_resets_at` feeds `week_fable`'s own `resets_at` — the
    /// marker-staleness tests pass a real epoch here so `classify_with` can
    /// judge whether a prior `<sid>.model-fallback` marker belongs to the
    /// CURRENT weekly window or an earlier one that has since rolled over,
    /// entirely from this in-memory reading (no separate usage-cache read;
    /// see `detect::LimitHit`'s doc). Every other test passes `None`, where
    /// the exact epoch doesn't matter.
    fn week_fable_capped_capture(
        profile_dir: &str,
        week_fable_resets_at: Option<i64>,
    ) -> StatuslineCapture {
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
                week_fable: Some(UsageSection {
                    pct: 100,
                    resets: None,
                    resets_at: week_fable_resets_at,
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
            "smart_dir must not be created when statusline_limit_hit_default is None"
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

    // ── run_from_statusline: fable-cap same-account model fallback ───────────

    /// A week_fable-only cap must relaunch the SAME profile with
    /// `model_override: Some("opus")`, must NOT touch `.switched`, and must
    /// NOT bump the sidecar hop — the account-switch machinery stays
    /// completely untouched by a relaunch that never left the account.
    #[test]
    fn run_from_statusline_fable_cap_relaunches_same_profile_with_model_override() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-fallback-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");

        let sentinel =
            crate::platform::relaunch::read_relaunch(&smart_dir.join(format!("{sid}.relaunch")))
                .expect("relaunch sentinel readable")
                .expect("relaunch sentinel present");
        assert_eq!(
            sentinel.target_profile, "limited",
            "a model fallback stays on the current profile, not a switch target"
        );
        assert_eq!(sentinel.model_override.as_deref(), Some("opus"));
        assert_eq!(sentinel.hop, 0, "a model fallback must not bump the hop");

        assert!(
            smart_dir.join(format!("{sid}.model-fallback")).exists(),
            "the one-shot model-fallback marker must be claimed"
        );
        assert!(
            !smart_dir.join(format!("{sid}.switched")).exists(),
            "a model fallback must never touch .switched — it would burn the \
             session's account-switch budget for a relaunch that stayed on \
             the same account"
        );
        assert!(
            !smart_dir.join(".last-switch").exists(),
            "a model fallback must never stamp the machine-wide cooldown — \
             it consumes no shared resource and must not throttle another \
             session's real account switch (classify_with's step 9 skips \
             cooldown_blocks entirely when fallback_model.is_some())"
        );
    }

    /// Kill-switch 1c (`.switched`) exists to stop account-switch loops, not
    /// to block the fallback: a session that already switched accounts once
    /// this run must still fall back on a later `week_fable` trip. Before
    /// this behaviour, a `.switched` session landing on a Fable-saturated
    /// account (now a pickable target) would be stranded on a capped Fable
    /// model with no rescue.
    #[test]
    fn run_from_statusline_fable_cap_falls_back_even_with_switched_marker_present() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-switched-then-fable-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        std::fs::write(smart_dir.join(format!("{sid}.switched")), "1700000000")
            .expect("pre-write the .switched marker as if a prior hop already fired");

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let sentinel =
            crate::platform::relaunch::read_relaunch(&smart_dir.join(format!("{sid}.relaunch")))
                .expect("relaunch sentinel readable")
                .expect("a pre-existing .switched marker must not block the fallback");
        assert_eq!(sentinel.target_profile, "limited");
        assert_eq!(
            sentinel.model_override.as_deref(),
            Some("opus"),
            "the fallback must still fire even though .switched already exists"
        );
    }

    /// The flip side: a `.switched` marker still blocks an ordinary
    /// (non-fallback) trip exactly as before — only a `WeekFable` dimension
    /// passes kill-switch 1c. A `week_all` cap on a session that already
    /// switched must stay `Decision::Skip`, never a second account switch.
    #[test]
    fn run_from_statusline_week_all_cap_still_blocked_by_switched_marker() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_one_viable_target());
        let sid = "sid-switched-then-week-all-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        std::fs::write(smart_dir.join(format!("{sid}.switched")), "1700000000")
            .expect("pre-write the .switched marker as if a prior hop already fired");

        let capture = capped_capture("/Users/example/.claude.limited");
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        assert!(
            !smart_dir.join(format!("{sid}.relaunch")).exists(),
            "a week_all trip on an already-switched session must stay blocked \
             by kill-switch 1c, exactly as before this fallback existed"
        );
        // The pre-existing `.switched` file would ALSO make the later,
        // unconditional exclusive claim in `run_from_statusline` fail on its
        // own (it noclobber-creates the same path) — so the absence of
        // `.relaunch` alone cannot tell kill-switch 1c firing early apart
        // from 1c doing nothing and the claim failing instead further down.
        // `.last-switch` (the machine-wide cooldown) is only ever touched at
        // step 9, well after 1c's step 4b — so its absence is the real
        // proof classify_with returned `Decision::Skip` at 4b and never
        // reached the claim at all.
        assert!(
            !smart_dir.join(".last-switch").exists(),
            "1c must return Skip at step 4b, before the cooldown at step 9 \
             is ever touched"
        );
    }

    /// A second `week_fable` trip after the one-shot fallback already fired,
    /// with its marker still fresh (same weekly window), is suppressed
    /// entirely: no new relaunch, no notify, no account switch. `week_fable`
    /// stays capped for days, so falling through to the ordinary
    /// account-switch path on the very next tick would undo the fallback
    /// within seconds of it firing, and the fallback itself already logged
    /// when it fired, so the repeat trip has nothing new to say. Uses
    /// `usage_with_one_viable_target` (a *different*, healthy profile
    /// exists) precisely to prove the switch does NOT happen even though a
    /// target is available.
    #[test]
    fn run_from_statusline_fable_cap_second_trip_is_suppressed_within_same_window() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_one_viable_target());
        let sid = "sid-fable-second-trip-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);

        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        let relaunch_path = smart_dir.join(format!("{sid}.relaunch"));
        let first_sentinel = crate::platform::relaunch::read_relaunch(&relaunch_path)
            .expect("relaunch sentinel readable")
            .expect("relaunch sentinel present after the first (fallback) trip");
        assert_eq!(first_sentinel.target_profile, "limited");
        assert_eq!(first_sentinel.model_override.as_deref(), Some("opus"));
        assert!(smart_dir.join(format!("{sid}.model-fallback")).exists());

        // The first commit's `commit_and_stop` stopped the managed process
        // (SIGTERM) as its real supervisor would, and in production the
        // relaunch loop then resumes the SAME session under a NEW managed
        // process before the next statusline tick. Simulate that resume so
        // the second tick's managed-session gate (step 8) sees a live
        // process, same as it would for real.
        spawn_fake_managed_process(&mut fixture, sid);

        // Second tick, same session, same still-capped week_fable reading —
        // exactly what the statusline loop produces about a second later.
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let second_sentinel = crate::platform::relaunch::read_relaunch(&relaunch_path)
            .expect("relaunch sentinel readable")
            .expect("relaunch sentinel present (unchanged) after the second (suppressed) trip");
        assert_eq!(
            second_sentinel.target_profile, "limited",
            "a suppressed second trip must leave the first fallback's sentinel untouched"
        );
        assert_eq!(
            second_sentinel.model_override.as_deref(),
            Some("opus"),
            "no new commit happened, so the sentinel is still the first fallback's"
        );
        assert_eq!(
            second_sentinel.hop, 0,
            "a suppressed trip must not bump the hop"
        );
        assert!(
            !smart_dir.join(format!("{sid}.switched")).exists(),
            "a suppressed second trip must never claim .switched — no account switch happened"
        );

        let log_path = smart_dir.join("limit-switch.log");
        let log = std::fs::read_to_string(&log_path).expect("limit-switch.log written");
        assert_eq!(
            log.lines().filter(|l| l.contains("limit-switch")).count(),
            1,
            "only the FIRST trip's commit should log a limit-switch line: {log:?}"
        );
        assert_eq!(
            log.lines().count(),
            1,
            "a suppressed second trip must be fully silent, not even a notify-only line: {log:?}"
        );
    }

    /// A `week_fable` cap on a session csm is not supervising (no `.pid`
    /// file, so `managed_session` reads `NotManaged`) never commits — step 8
    /// returns before any state mutation. Its notify text must name the
    /// model fallback the user should resume with by hand, not tell them to
    /// "switch to" the account they are already on, and must include
    /// `--model` so the manual command actually escapes the capped model.
    #[test]
    fn run_from_statusline_fable_cap_unmanaged_session_names_the_model_fallback() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        // No `.pid` file for this sid — `managed_session` reads `NotManaged`.
        let fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-unmanaged-0001";
        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);

        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        assert!(
            !smart_dir.join(format!("{sid}.relaunch")).exists(),
            "an unmanaged session must never commit a relaunch"
        );

        let log = std::fs::read_to_string(smart_dir.join("limit-switch.log"))
            .expect("limit-switch.log written");
        assert!(
            log.contains("resume on model [opus]"),
            "must name the model fallback, not an account switch: {log:?}"
        );
        assert!(
            log.contains(&format!(
                "csm --profile limited --resume {} --model opus",
                sid_short(sid)
            )),
            "must give a manual command that names --model, or resuming it \
             would land back on the capped model: {log:?}"
        );
        assert!(
            !log.contains("switch to [limited]"),
            "must not tell the user to switch to the account they are already on: {log:?}"
        );
    }

    /// After the model fallback has fired, `.detected` (the notify-only
    /// dedup slot) must still be free: 5c's `Decision::Skip` on a repeat
    /// `week_fable` trip must not consume it, or a later, genuinely new
    /// notify-only reason for the same session would be silently swallowed
    /// for good.
    #[test]
    fn suppressed_fable_repeat_trip_does_not_block_a_later_genuine_notify() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-then-notify-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let fable_capture = week_fable_capped_capture("/Users/example/.claude.limited", None);

        // Trip 1: the fallback fires and commits (`LimitSwitch` — never
        // touches `.detected`).
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &fable_capture,
        );
        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        assert!(
            smart_dir.join(format!("{sid}.relaunch")).exists(),
            "trip 1 must commit the fallback"
        );

        spawn_fake_managed_process(&mut fixture, sid);

        // Trip 2: a repeat `week_fable` trip with the marker still fresh
        // hits 5c and must be `Decision::Skip` — silent, and critically must
        // not consume `.detected`; a notify-only here would swallow a later
        // genuine notify-only.
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &fable_capture,
        );

        // Trip 3: an unrelated, genuinely new notify-only reason (week_all
        // capped, no viable target) for the SAME session must still fire —
        // proof `.detected` was never spent by trip 2's silent suppression.
        let week_all_capture = capped_capture("/Users/example/.claude.limited");
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &week_all_capture,
        );

        let log_path = smart_dir.join("limit-switch.log");
        let log = std::fs::read_to_string(&log_path).expect("limit-switch.log written");
        let notify_lines: Vec<&str> = log.lines().filter(|l| l.contains("notify-only")).collect();
        assert_eq!(
            notify_lines.len(),
            1,
            "exactly one notify-only line — trip 2's suppression must be fully \
             silent, not a second notify-only: {log:?}"
        );
        assert!(
            notify_lines[0].contains("week_all"),
            "trip 3's notify-only must name week_all, the dimension that actually \
             tripped it: {log:?}"
        );
        assert!(
            !notify_lines.iter().any(|l| l.contains("week_fable")),
            "no notify-only line may name week_fable — trip 2's suppressed repeat \
             must never have produced one: {log:?}"
        );
        assert_eq!(
            log.lines().filter(|l| l.contains("limit-switch")).count(),
            1,
            "only trip 1 should have logged a limit-switch line: {log:?}"
        );
    }

    /// The suppression above is bounded, not permanent: once the
    /// `week_fable` weekly window has rolled over past the marker's epoch,
    /// `model_fallback_marker_is_stale` reads it as absent and the fallback
    /// fires again for the NEW window, still on the same account (never an
    /// account switch). The reset epoch rides on the `StatuslineCapture`
    /// itself now, so this test needs no usage-cache rewrite between ticks —
    /// only a second capture with a later `week_fable_resets_at`.
    #[test]
    fn run_from_statusline_fable_cap_fires_again_after_marker_goes_stale() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        // No viable switch target at all, so a real account switch would
        // have produced a notify-only ("no headroom"), not another fallback
        // sentinel — this distinguishes "fired again" from "silently did
        // nothing".
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-stale-marker-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        // First trip: no marker exists yet, so `already_fell_back` is false
        // regardless of the exact epoch — any fixed epoch well into the
        // future works. The real test is the SECOND tick below, computed
        // off the marker's own (real wall-clock, `stop.rs`'s own
        // `now_epoch`) written value.
        let first_capture =
            week_fable_capped_capture("/Users/example/.claude.limited", Some(4_000_000_000));
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &first_capture,
        );

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        let relaunch_path = smart_dir.join(format!("{sid}.relaunch"));
        assert!(relaunch_path.exists(), "first trip must commit a fallback");
        let marker_path = smart_dir.join(format!("{sid}.model-fallback"));
        assert!(
            std::fs::read_to_string(&marker_path).is_ok(),
            "first trip must write the marker"
        );

        // Remove the first trip's sentinel and pin the marker to a known,
        // long-past epoch — nothing beyond this line depends on real
        // wall-clock timing. If the second tick were a no-op (a suppressed
        // repeat, or staleness wrongly judged false), `relaunch_path` would
        // stay absent and `marker_path` would still hold `OLD_MARKER_EPOCH`
        // exactly, so both checks below would catch it.
        const OLD_MARKER_EPOCH: i64 = 1_000_000_000; // 2001-09-09, long before any real run
        std::fs::remove_file(&relaunch_path).expect("remove first trip's sentinel");
        // Same profile ("limited") as the session is still on — this pins
        // the STALENESS axis specifically, not the separate "different
        // profile" axis `model_fallback_marker_is_current` also checks.
        std::fs::write(&marker_path, format!("{OLD_MARKER_EPOCH} limited"))
            .expect("rewrite marker with an old epoch");

        spawn_fake_managed_process(&mut fixture, sid);

        // Second tick's reading names a window that ends well over 7 days
        // after the pinned marker epoch — i.e. the marker was written during
        // an EARLIER window.
        let second_capture = week_fable_capped_capture(
            "/Users/example/.claude.limited",
            Some(OLD_MARKER_EPOCH + 8 * 86_400),
        );
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &second_capture,
        );

        // The decision must treat the marker as stale and fall back again
        // rather than staying suppressed or switching accounts. Since the
        // first sentinel was deleted above, this only passes if the second
        // tick actually committed a NEW one.
        let sentinel = crate::platform::relaunch::read_relaunch(&relaunch_path)
            .expect("relaunch sentinel readable")
            .expect("a fresh fallback must commit a new sentinel");
        assert_eq!(sentinel.target_profile, "limited");
        assert_eq!(
            sentinel.model_override.as_deref(),
            Some("opus"),
            "once the marker is stale, the fallback fires again on the SAME account"
        );
        assert!(
            !smart_dir.join(format!("{sid}.switched")).exists(),
            "a renewed fallback must never claim .switched — it is still not an account switch"
        );

        // `classify_with` removed the stale leftover marker the moment it
        // judged it stale, so the renewed fallback's exclusive claim
        // (`stop::claim_model_fallback`) lands on an empty slot rather than
        // losing to the pinned old one. Comparing against the exact pinned
        // value (rather than "greater or equal", which a no-op would also
        // satisfy since the file would be untouched) proves the second
        // commit really happened.
        let second_marker_content = std::fs::read_to_string(&marker_path)
            .expect("marker still present after the renewed fallback");
        let second_marker_epoch: i64 = second_marker_content
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .expect("marker holds an epoch");
        assert_ne!(
            second_marker_epoch, OLD_MARKER_EPOCH,
            "the renewed fallback must overwrite the marker with a current epoch"
        );
        assert!(
            second_marker_content.trim().ends_with("limited"),
            "the renewed marker must still record the current profile: {second_marker_content:?}"
        );

        // Each fresh fallback logs its own limit-switch line; a suppressed
        // repeat (5c) would not, so counting two lines here rules out a
        // silently-passing no-op on the second tick.
        let log_path = smart_dir.join("limit-switch.log");
        let log = std::fs::read_to_string(&log_path).expect("limit-switch.log written");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("limit-switch") && l.contains("model-fallback=opus"))
                .count(),
            2,
            "both the first fallback and the renewed one after staleness must log: {log:?}"
        );
    }

    /// A `.model-fallback` marker written for a DIFFERENT profile must never
    /// suppress a fallback on the CURRENT one: the marker belongs to the
    /// account it was written on, not to the session id alone. Without this,
    /// a session that switched accounts once (replaying the same sidecar
    /// model onto the new account, since the fallback never touches it)
    /// would find the old account's still-fresh marker and stay silently
    /// stranded on the new account's own capped Fable model.
    #[test]
    fn run_from_statusline_fable_cap_foreign_profile_marker_does_not_suppress() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-foreign-marker-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        let marker_path = smart_dir.join(format!("{sid}.model-fallback"));
        // 3 days before the reset this tick reports — comfortably inside
        // the current 7-day window (so `model_fallback_marker_is_stale`
        // alone reads it as fresh), but written for "other" — an account
        // this session is NOT on. This isolates the profile axis from the
        // separate staleness axis: only the profile mismatch can explain a
        // suppression here.
        const RESETS_AT: i64 = 4_100_000_000;
        std::fs::write(&marker_path, format!("{} other", RESETS_AT - 3 * 86_400))
            .expect("pre-write a marker for a different profile");

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", Some(RESETS_AT));
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let sentinel =
            crate::platform::relaunch::read_relaunch(&smart_dir.join(format!("{sid}.relaunch")))
                .expect("relaunch sentinel readable")
                .expect("a foreign-profile marker must not suppress the fallback");
        assert_eq!(sentinel.target_profile, "limited");
        assert_eq!(
            sentinel.model_override.as_deref(),
            Some("opus"),
            "the fallback must fire on the current account despite the foreign marker"
        );

        // The fallback rewrites the marker for the CURRENT profile — a
        // reader a moment from now must see this account's own claim, not
        // the leftover "other" one.
        let content =
            std::fs::read_to_string(&marker_path).expect("marker present after the fallback");
        assert!(
            content.trim().ends_with("limited"),
            "the marker must now record the current profile, not the foreign one: {content:?}"
        );
        assert!(
            !content.trim().ends_with("other"),
            "the foreign profile's claim must not survive: {content:?}"
        );
    }

    /// An unparseable `.model-fallback` marker (a crash or a torn read
    /// leaving a corrupt or empty file) must not permanently bar the
    /// session. Pre-seed the marker with garbage before the tick fires; the
    /// fallback must still fire and leave behind a fresh, valid, parseable
    /// marker — proof the session self-healed instead of reading the
    /// corrupt file as "already fell back" and staying stuck there forever.
    #[test]
    fn run_from_statusline_fable_cap_fires_when_marker_is_corrupt() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-corrupt-marker-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        let marker_path = smart_dir.join(format!("{sid}.model-fallback"));
        std::fs::write(&marker_path, b"").expect("seed an empty/corrupt marker");

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let sentinel =
            crate::platform::relaunch::read_relaunch(&smart_dir.join(format!("{sid}.relaunch")))
                .expect("relaunch sentinel readable")
                .expect("a corrupt marker must not block the fallback from firing");
        assert_eq!(sentinel.target_profile, "limited");
        assert_eq!(
            sentinel.model_override.as_deref(),
            Some("opus"),
            "a corrupt marker must read as absent, not as an account-switch fall-through"
        );

        let content =
            std::fs::read_to_string(&marker_path).expect("marker present after self-heal");
        let mut fields = content.split_whitespace();
        assert!(
            fields
                .next()
                .is_some_and(|epoch| epoch.parse::<i64>().is_ok()),
            "the self-healed marker must hold a valid, complete epoch: {content:?}"
        );
        assert_eq!(
            fields.next(),
            Some("limited"),
            "the self-healed marker must record the current profile: {content:?}"
        );
    }

    /// An overlapping claim that loses the race must never touch a marker
    /// a different, already-successful claim+commit wrote.
    /// `run_from_statusline`'s `if !claimed { return; }` guard (in the
    /// `LimitSwitch` arm above) means a losing claim never reaches
    /// `commit_and_stop` or its release-on-commit-failure cleanup at all —
    /// this drives the exact two primitives that arm calls, in the order two
    /// overlapping statusline ticks for the same session would: claim +
    /// commit for the winner, then a second claim attempt for the loser.
    /// A losing claim must return `false` so it never reaches
    /// `commit_and_stop` or the release-on-failure cleanup that would erase
    /// the winner's marker.
    #[test]
    fn overlapping_model_fallback_claim_never_erases_a_committed_marker() {
        let home = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let sid = "sid-fable-overlap-0001";
            let smart_dir = home.path().join(".claude.shared").join("smart");
            std::fs::create_dir_all(&smart_dir).unwrap();

            // Tick A: wins the claim and commits successfully — no pidfile
            // is needed for `commit_and_stop` to succeed (an unmanaged
            // session is a no-op stop, not a failure).
            assert!(
                stop::claim_model_fallback(sid, "limited"),
                "tick A must win the claim"
            );
            stop::commit_and_stop(sid, "limited", "resume", "/tmp/proj", 0, Some("opus"))
                .expect("tick A's commit succeeds");
            let marker_path = smart_dir.join(format!("{sid}.model-fallback"));
            let after_a = std::fs::read_to_string(&marker_path).expect("marker written by tick A");

            // Tick B: an overlapping claim for the SAME session must lose.
            assert!(
                !stop::claim_model_fallback(sid, "limited"),
                "tick B must lose the claim — tick A already holds the marker"
            );
            let after_b = std::fs::read_to_string(&marker_path).expect("marker still present");
            assert_eq!(
                after_a, after_b,
                "a losing claim must never disturb the marker the winner wrote"
            );
        });
    }

    /// A week_all cap (an ordinary account-saturation signal, not a
    /// model-scoped one) must still switch accounts exactly as it always
    /// has: no `model_override`, hop bumped, `.switched` claimed.
    #[test]
    fn run_from_statusline_week_all_cap_still_switches_accounts() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_one_viable_target());
        let sid = "sid-week-all-switch-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let capture = capped_capture("/Users/example/.claude.limited");
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");

        let sentinel =
            crate::platform::relaunch::read_relaunch(&smart_dir.join(format!("{sid}.relaunch")))
                .expect("relaunch sentinel readable")
                .expect("relaunch sentinel present");
        assert_eq!(sentinel.target_profile, "healthy");
        assert_eq!(
            sentinel.model_override, None,
            "an account-level cap must not set a model override"
        );
        assert_eq!(sentinel.hop, 1, "an account switch must bump the hop");

        assert!(smart_dir.join(format!("{sid}.switched")).exists());
        assert!(
            !smart_dir.join(format!("{sid}.model-fallback")).exists(),
            "an account switch must never claim the model-fallback marker"
        );
    }

    /// A model fallback's commit must leave the sidecar file byte-identical
    /// to what it was before — `commit_and_stop`'s `model_override: Some(_)`
    /// branch skips `merge_sidecar_hop` entirely (see its doc comment), so a
    /// sidecar with pre-existing `passthru` flags must round-trip untouched,
    /// not just "hop unchanged".
    #[test]
    fn run_from_statusline_fable_cap_leaves_sidecar_byte_identical() {
        let _guard_cmd = crate::testenv::lock_for("CSM_USAGE_CMD");
        let _guard_bin = crate::testenv::lock_for("CLAUDE_SMART_CLAUDE_BIN");
        let mut fixture = isolated_env(&usage_with_no_viable_target());
        let sid = "sid-fable-sidecar-0001";
        spawn_fake_managed_process(&mut fixture, sid);

        let smart_dir = fixture.home.path().join(".claude.shared").join("smart");
        std::fs::create_dir_all(&smart_dir).expect("create smart_dir");
        let sidecar_path = smart_dir.join(format!("{sid}.json"));
        let sidecar = crate::sidecar::Sidecar {
            session_id: Some(sid.to_string()),
            profile: Some("limited".to_string()),
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/tmp/proj".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]),
            hop: Some(crate::sidecar::Sidecar::hop_value(0)),
            ..Default::default()
        };
        crate::sidecar::write_sidecar(&sidecar_path, &sidecar).expect("write fixture sidecar");
        let before = std::fs::read(&sidecar_path).expect("read sidecar before");

        let capture = week_fable_capped_capture("/Users/example/.claude.limited", None);
        run_from_statusline(
            &format!(r#"{{"session_id": "{sid}", "cwd": "/tmp/proj"}}"#),
            &capture,
        );

        let after = std::fs::read(&sidecar_path).expect("read sidecar after");
        assert_eq!(
            before, after,
            "a model fallback must never rewrite the sidecar file at all"
        );
    }
}
